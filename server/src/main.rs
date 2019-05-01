#[global_allocator]
static ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate log;

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use getopts::Options;

use itertools::Itertools;

use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::operators::generic::OutputHandle;
use timely::dataflow::operators::{Operator, Probe};
use timely::logging::{Logger, TimelyEvent};
use timely::synchronization::Sequencer;

use differential_dataflow::logging::DifferentialEvent;
use differential_dataflow::operators::Consolidate;

use mio::net::TcpListener;
use mio::*;

use slab::Slab;

use ws::connection::{ConnEvent, Connection};

use declarative_dataflow::server::{Config, CreateAttribute, Request, Server, TxId};
use declarative_dataflow::sinks::{Sinkable, SinkingContext};
use declarative_dataflow::timestamp::Coarsen;
use declarative_dataflow::{Eid, Error, Output, ResultDiff};

/// Server timestamp type.
#[cfg(all(not(feature = "real-time"), not(feature = "bitemporal")))]
type T = u64;

/// Server timestamp type.
#[cfg(feature = "real-time")]
type T = Duration;

#[cfg(feature = "bitemporal")]
use declarative_dataflow::timestamp::pair::Pair;
/// Server timestamp type.
#[cfg(feature = "bitemporal")]
type T = Pair<Duration, u64>;

const SERVER: Token = Token(std::usize::MAX - 1);
const RESULTS: Token = Token(std::usize::MAX - 2);
const SYSTEM: Token = Token(std::usize::MAX - 3);

/// A mutation of server state.
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Serialize, Deserialize, Debug)]
struct Command {
    /// The worker that received this command from a client originally
    /// and is therefore the one that should receive all outputs.
    pub owner: usize,
    /// The client token that issued the command. Only relevant to the
    /// owning worker, as no one else has the connection.
    pub client: usize,
    /// Requests issued by the client.
    pub requests: Vec<Request>,
}

fn main() {
    env_logger::init();

    let mut opts = Options::new();
    opts.optopt("", "port", "server port", "PORT");
    opts.optflag(
        "",
        "manual-advance",
        "forces clients to call AdvanceDomain explicitely",
    );
    opts.optflag("", "enable-logging", "enable log event sources");
    opts.optflag("", "enable-history", "enable historical queries");
    opts.optflag("", "enable-optimizer", "enable WCO queries");
    opts.optflag("", "enable-meta", "enable queries on the query graph");

    let args: Vec<String> = std::env::args().collect();
    let timely_args = std::env::args().take_while(|ref arg| *arg != "--");
    let timely_config = timely::Configuration::from_args(timely_args).unwrap();

    timely::execute(timely_config, move |worker| {
        // read configuration
        let server_args = args.iter().rev().take_while(|arg| *arg != "--");
        let default_config: Config = Default::default();
        let config = match opts.parse(server_args) {
            Err(err) => panic!(err),
            Ok(matches) => {
                let starting_port = matches
                    .opt_str("port")
                    .map(|x| x.parse().unwrap_or(default_config.port))
                    .unwrap_or(default_config.port);

                Config {
                    port: starting_port + (worker.index() as u16),
                    manual_advance: matches.opt_present("manual-advance"),
                    enable_logging: matches.opt_present("enable-logging"),
                    enable_optimizer: matches.opt_present("enable-optimizer"),
                    enable_meta: matches.opt_present("enable-meta"),
                }
            }
        };

        // setup interpretation context
        let mut server = Server::<T, Token>::new_at(config.clone(), worker.timer());

        if config.enable_logging {
            #[cfg(feature = "real-time")]
            server.enable_logging(worker).unwrap();
        }

        // The server might specify a sequence of requests for
        // setting-up built-in arrangements. We serialize those here
        // and pre-load the sequencer with them, such that they will
        // flow through the regular request handling.
        let builtins = Server::<T, Token>::builtins();
        let preload_command = Command {
            owner: worker.index(),
            client: SYSTEM.0,
            requests: builtins,
        };

        // setup serialized command queue (shared between all workers)
        let mut sequencer: Sequencer<Command> =
            Sequencer::preloaded(worker, Instant::now(), VecDeque::from(vec![preload_command]));

        // configure websocket server
        let ws_settings = ws::Settings {
            max_connections: 1024,
            ..ws::Settings::default()
        };

        // setup results channel
        let (send_results, recv_results) = mio_extras::channel::channel::<Output<T>>();

        // setup server socket
        // let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), config.port);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0,0,0,0)), config.port);
        let server_socket = TcpListener::bind(&addr).unwrap();
        let mut connections = Slab::with_capacity(ws_settings.max_connections);
        let mut next_connection_id: u32 = 0;

        // setup event loop
        let poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(1024);

        poll.register(
            &recv_results,
            RESULTS,
            Ready::readable(),
            PollOpt::edge() | PollOpt::oneshot(),
        ).unwrap();

        poll.register(&server_socket, SERVER, Ready::readable(), PollOpt::level())
            .unwrap();

        info!(
            "[WORKER {}] running with config {:?}, {} peers",
            worker.index(),
            config,
            worker.peers(),
        );

        // Sequence counter for commands.
        let mut next_tx: TxId = 0;

        let mut shutdown = false;

        // Keep a buffer around into which all connection events can
        // be dropped without having to allocate all over the place.
        let mut conn_events = Vec::new();

        while !shutdown {
            // each worker has to...
            //
            // ...accept new client connections
            // ...accept commands on a client connection and push them to the sequencer
            // ...step computations
            // ...send results to clients
            //
            // by having everything inside a single event loop, we can
            // easily make trade-offs such as limiting the number of
            // commands consumed, in order to ensure timely progress
            // on registered queues

            // polling - should usually be driven completely
            // non-blocking (i.e. timeout 0), but higher timeouts can
            // be used for debugging or artificial braking

            if server.scheduler.borrow().has_pending() {
                let mut scheduler = server.scheduler.borrow_mut();
                while let Some(activator) = scheduler.next() {
                    activator.activate();
                }
            } else {
                // @TODO in blocking mode, we could check whether
                // worker 'would park', and block for input here
                // poll.poll(&mut events, None).expect("failed to poll I/O events");
            }

            // We mustn't timeout here, operators are pending!
            poll.poll(&mut events, Some(Duration::from_millis(0)))
                .expect("failed to poll I/O events");

            for event in events.iter() {
                trace!(
                    "[WORKER {}] recv event on {:?}",
                    worker.index(),
                    event.token()
                );

                match event.token() {
                    SERVER => {
                        if event.readiness().is_readable() {
                            // new connection arrived on the server socket
                            match server_socket.accept() {
                                Err(err) => error!(
                                    "[WORKER {}] error while accepting connection {:?}",
                                    worker.index(),
                                    err
                                ),
                                Ok((socket, addr)) => {
                                    let token = {
                                        let entry = connections.vacant_entry();
                                        let token = Token(entry.key());
                                        let connection_id = next_connection_id;
                                        next_connection_id = next_connection_id.wrapping_add(1);

                                        entry.insert(Connection::new(
                                            token,
                                            socket,
                                            ws_settings,
                                            connection_id,
                                        ));

                                        token
                                    };

                                    info!(
                                        "[WORKER {}] new tcp connection from {} (token {:?})",
                                        worker.index(),
                                        addr,
                                        token
                                    );

                                    let conn = &mut connections[token.into()];

                                    conn.as_server().unwrap();

                                    poll.register(
                                        conn.socket(),
                                        conn.token(),
                                        conn.events(),
                                        PollOpt::edge() | PollOpt::oneshot(),
                                    ).unwrap();
                                }
                            }
                        }
                    }
                    RESULTS => {
                        while let Ok(out) = recv_results.try_recv() {
                            let tokens: Box<dyn Iterator<Item=Token>> = match &out {
                                &Output::QueryDiff(ref name, ref results) => {
                                    info!("[WORKER {}] {} {} results", worker.index(), name, results.len());

                                    match server.interests.get(name) {
                                        None => {
                                            warn!("result on query {} w/o interested clients", name);
                                            Box::new(std::iter::empty())
                                        }
                                        Some(tokens) => Box::new(tokens.iter().cloned()),
                                    }
                                }
                                &Output::TenantDiff(ref name, client, ref results) => {
                                    info!("[WORKER {}] {} results for tenant {:?} on query {}", worker.index(), results.len(), client, name);
                                    Box::new(std::iter::once(client.into()))
                                }
                                &Output::Json(ref name, _, _, _) => {
                                    info!("[WORKER {}] json on query {}", worker.index(), name);

                                    match server.interests.get(name) {
                                        None => {
                                            warn!("result on query {} w/o interested clients", name);
                                            Box::new(std::iter::empty())
                                        }
                                        Some(tokens) => Box::new(tokens.iter().cloned()),
                                    }
                                }
                                &Output::Message(client, ref msg) => {
                                    info!("[WORKER {}] {:?}", worker.index(), msg);
                                    Box::new(std::iter::once(client.into()))
                                }
                                &Output::Error(client, ref error, _) => {
                                    error!("[WORKER {}] {:?}", worker.index(), error);
                                    Box::new(std::iter::once(client.into()))
                                }
                            };

                            let serialized = serde_json::to_string::<Output<T>>(&out)
                                .expect("failed to serialize output");

                            let msg = ws::Message::text(serialized);

                            for token in tokens {
                                match connections.get_mut(token.into()) {
                                    None => {
                                        warn!("client {:?} has gone away undetected, notifying", token);
                                        sequencer.push(Command {
                                            owner: worker.index(),
                                            client: token.into(),
                                            requests: vec![Request::Disconnect],
                                        });
                                    }
                                    Some(conn) => {
                                        conn.send_message(msg.clone())
                                            .expect("failed to send message");

                                        poll.reregister(
                                            conn.socket(),
                                            conn.token(),
                                            conn.events(),
                                            PollOpt::edge() | PollOpt::oneshot(),
                                        ).unwrap();
                                    }
                                }
                            }
                        }

                        poll.reregister(
                            &recv_results,
                            RESULTS,
                            Ready::readable(),
                            PollOpt::edge() | PollOpt::oneshot(),
                        ).unwrap();
                    }
                    _ => {
                        let token = event.token();
                        let active = {
                            let event_readiness = event.readiness();

                            let conn_readiness = connections[token.into()].events();
                            if (event_readiness & conn_readiness).is_readable() {
                                if let Err(err) = connections[token.into()].read(&mut conn_events) {
                                    trace!(
                                        "[WORKER {}] error while reading: {}",
                                        worker.index(),
                                        err
                                    );
                                    // @TODO error handling
                                    connections[token.into()].error(err)
                                }
                            }

                            let conn_readiness = connections[token.into()].events();
                            if (event_readiness & conn_readiness).is_writable() {
                                if let Err(err) = connections[token.into()].write(&mut conn_events) {
                                    trace!(
                                        "[WORKER {}] error while writing: {}",
                                        worker.index(),
                                        err
                                    );
                                    // @TODO error handling
                                    connections[token.into()].error(err)
                                }
                            }

                            trace!("read {} connection events", conn_events.len());

                            for conn_event in conn_events.drain(..) {
                                match conn_event {
                                    ConnEvent::Message(msg) => {
                                        trace!("[WS] ConnEvent::Message");
                                        match msg {
                                            ws::Message::Text(string) => {
                                                match serde_json::from_str::<Vec<Request>>(&string) {
                                                    Err(serde_error) => {
                                                        send_results
                                                            .send(Output::Error(token.into(), Error::incorrect(serde_error), next_tx - 1))
                                                            .unwrap();
                                                    }
                                                    Ok(requests) => {
                                                        trace!("[WORKER {}] push command", worker.index());
                                                        sequencer.push(
                                                            Command {
                                                                owner: worker.index(),
                                                                client: token.into(),
                                                                requests,
                                                            }
                                                        );
                                                    }
                                                }
                                            }
                                            ws::Message::Binary(bytes) => {
                                                match rmp_serde::decode::from_slice::<Vec<Request>>(&bytes) {
                                                    Err(rmp_error) => {
                                                        send_results
                                                            .send(Output::Error(token.into(), Error::incorrect(rmp_error), next_tx - 1))
                                                            .unwrap();
                                                    }
                                                    Ok(requests) => {
                                                        trace!("[WORKER {}] push binary command", worker.index());
                                                        sequencer.push(
                                                            Command {
                                                                owner: worker.index(),
                                                                client: token.into(),
                                                                requests,
                                                            }
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    ConnEvent::Close(code, reason) => {
                                        trace!("[WS] ConnEvent::Close");
                                        info!("[WORKER {}] connection closing (token {:?}, {:?}, {})", worker.index(), token, code, reason);
                                    }
                                    other => {
                                        trace!("[WS] {:?}", other);
                                    }
                                }
                            }

                            // connection events may have changed
                            connections[token.into()].events().is_readable()
                                || connections[token.into()].events().is_writable()
                        };

                        // NOTE: Closing state only applies after a ws connection was successfully
                        // established. It's possible that we may go inactive while in a connecting
                        // state if the handshake fails.
                        if !active {
                            info!("[WORKER {}] token={:?} disconnected", worker.index(), token);

                            sequencer.push(Command {
                                owner: worker.index(),
                                client: token.into(),
                                requests: vec![Request::Disconnect],
                            });

                            connections.remove(token.into());
                        } else {
                            let conn = &connections[token.into()];
                            poll.reregister(
                                conn.socket(),
                                conn.token(),
                                conn.events(),
                                PollOpt::edge() | PollOpt::oneshot(),
                            ).unwrap();
                        }
                    }
                }
            }

            // handle commands

            while let Some(mut command) = sequencer.next() {

                // Count-up sequence numbers.
                next_tx += 1;

                info!("[WORKER {}] {} requests by client {} at {}", worker.index(), command.requests.len(), command.client, next_tx);

                let owner = command.owner;
                let client = command.client;
                let last_tx = next_tx - 1;

                for req in command.requests.drain(..) {

                    // @TODO only create a single dataflow, but only if req != Transact

                    trace!("[WORKER {}] {:?}", worker.index(), req);

                    match req {
                        Request::Transact(req) => {
                            if let Err(error) = server.transact(req, owner, worker.index()) {
                                send_results.send(Output::Error(client, error, last_tx)).unwrap();
                            }
                        }
                        Request::Interest(req) => {
                            let interests = server.interests
                                .entry(req.name.clone())
                                .or_insert_with(HashSet::new);

                            // We need to check this, because we only want to setup
                            // the dataflow on the first interest.
                            let was_first = interests.is_empty();

                            // All workers keep track of every client's interests, s.t. they
                            // know when to clean up unused dataflows.
                            interests.insert(Token(client));

                            // For multi-tenant flows, we need to keep track of the worker
                            // that is managing this client's connection.
                            if let Some(_) = req.tenant {
                                server.tenant_owner.borrow_mut().insert(Token(client), command.owner as u64);
                            }

                            if was_first {
                                let send_results_handle = send_results.clone();

                                let disable_logging = req.disable_logging.unwrap_or(false);
                                let mut timely_logger = None;
                                let mut differential_logger = None;

                                if disable_logging {
                                    info!("Disabling logging");
                                    timely_logger = worker.log_register().remove("timely");
                                    differential_logger = worker.log_register().remove("differential/arrange");
                                }

                                worker.dataflow::<T, _, _>(|scope| {
                                    let sink_context: SinkingContext = (&req).into();

                                    match server.interest(&req.name, scope) {
                                        Err(error) => {
                                            send_results.send(Output::Error(client, error, last_tx)).unwrap();
                                        }
                                        Ok(relation) => {
                                            let delayed = match req.granularity {
                                                None => relation,
                                                Some(granularity) => {
                                                    let granularity: T = granularity.into();
                                                    relation
                                                        .delay(move |t| t.coarsen(&granularity))
                                                        .consolidate()
                                                }
                                            };

                                            if let Some(offset) = req.tenant {
                                                let mut buffer = Vec::new();
                                                let tenant_owner = server.tenant_owner.clone();

                                                let pact = Exchange::new(move |(ref tuple, _t, _diff): &ResultDiff<T>| {
                                                    let tenant: Eid = tuple[offset].clone().into();
                                                    tenant_owner.borrow().get(&Token(tenant as usize)).unwrap().clone()
                                                });

                                                match req.sink {
                                                    Some(sink) => {
                                                        sink.sink(&delayed.inner, pact, &mut server.probe, sink_context)
                                                            .expect("sinking failed");
                                                    }
                                                    None => {
                                                        delayed
                                                            .inner
                                                            .unary(pact, "MultiTenantResults", move |_cap, _info| {
                                                                move |input, _output: &mut OutputHandle<_, ResultDiff<T>, _>| {
                                                                    input.for_each(|_time, data| {
                                                                        data.swap(&mut buffer);

                                                                        let per_tenant = buffer
                                                                            .drain(..)
                                                                            .group_by(|(tuple, _, _)| {
                                                                                let tenant: Eid = tuple[offset].clone().into();
                                                                                tenant as usize
                                                                            });

                                                                        for (tenant, batch) in &per_tenant {
                                                                            send_results_handle
                                                                                .send(Output::TenantDiff(sink_context.name.clone(), tenant, batch.collect()))
                                                                                .unwrap();
                                                                        }
                                                                    });
                                                                }
                                                            })
                                                            .probe_with(&mut server.probe);
                                                    }
                                                }
                                            } else {
                                                let pact = Exchange::new(move |_| owner as u64);

                                                match req.sink {
                                                    Some(sink) => {
                                                        let sunk = sink.sink(&delayed.inner, pact, &mut server.probe, sink_context)
                                                            .expect("sinking failed");

                                                        if let Some(sunk) = sunk {
                                                            let mut vector = Vec::new();
                                                            sunk
                                                                .unary(Pipeline, "SinkResults", move |_cap, _info| {
                                                                    move |input, _output: &mut OutputHandle<_, ResultDiff<T>, _>| {
                                                                        input.for_each(|_time, data| {
                                                                            data.swap(&mut vector);

                                                                            for out in vector.drain(..) {
                                                                                send_results_handle.send(out)
                                                                                    .unwrap();
                                                                            }
                                                                        });
                                                                    }
                                                                })
                                                                .probe_with(&mut server.probe);
                                                        }
                                                    }
                                                    None => {
                                                        delayed
                                                            .inner
                                                            .unary(pact, "ResultsRecv", move |_cap, _info| {
                                                                move |input, _output: &mut OutputHandle<_, ResultDiff<T>, _>| {
                                                                    // due to the exchange pact, this closure is only
                                                                    // executed by the owning worker

                                                                    // @TODO only forward inputs up to the frontier!

                                                                    input.for_each(|_time, data| {
                                                                        send_results_handle
                                                                            .send(Output::QueryDiff(sink_context.name.clone(), data.to_vec()))
                                                                            .unwrap();
                                                                    });
                                                                }
                                                            })
                                                            .probe_with(&mut server.probe);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                });

                                if disable_logging {
                                    if let Some(logger) = timely_logger {
                                        if let Ok(logger) = logger.downcast::<Logger<TimelyEvent>>() {
                                            worker
                                                .log_register()
                                                .insert_logger::<TimelyEvent>("timely", *logger);
                                        }
                                    }

                                    if let Some(logger) = differential_logger {
                                        if let Ok(logger) = logger.downcast::<Logger<DifferentialEvent>>() {
                                            worker
                                                .log_register()
                                                .insert_logger::<DifferentialEvent>("differential/arrange", *logger);
                                        }
                                    }
                                }
                            }
                        }
                        Request::Uninterest(name) => server.uninterest(Token(command.client), &name),
                        Request::Register(req) => {
                            if let Err(error) = server.register(req) {
                                send_results.send(Output::Error(client, error, last_tx)).unwrap();
                            }
                        }
                        Request::RegisterSource(source) => {
                            worker.dataflow::<T, _, _>(|scope| {
                                if let Err(error) = server.register_source(Box::new(source), scope) {
                                    send_results.send(Output::Error(client, error, last_tx)).unwrap();
                                }
                            });
                        }
                        Request::CreateAttribute(CreateAttribute { name, config }) => {
                            worker.dataflow::<T, _, _>(|scope| {
                                if let Err(error) = server.context.internal.create_transactable_attribute(&name, config, scope) {
                                    send_results.send(Output::Error(client, error, last_tx)).unwrap();
                                }
                            });
                        }
                        Request::AdvanceDomain(name, next) => {
                            if let Err(error) = server.advance_domain(name, next.into()) {
                                send_results.send(Output::Error(client, error, last_tx)).unwrap();
                            }
                        }
                        Request::CloseInput(name) => {
                            if let Err(error) = server.context.internal.close_input(name) {
                                send_results.send(Output::Error(client, error, last_tx)).unwrap();
                            }
                        }
                        Request::Disconnect => server.disconnect_client(Token(command.client)),
                        Request::Setup => unimplemented!(),
                        Request::Tick => {
                            #[cfg(all(not(feature = "real-time"), not(feature = "bitemporal")))]
                            let next = next_tx as u64;
                            #[cfg(feature = "real-time")]
                            let next = Instant::now().duration_since(worker.timer());
                            #[cfg(feature = "bitemporal")]
                            let next = Pair::new(Instant::now().duration_since(worker.timer()), next_tx as u64);

                            if let Err(error) = server.context.internal.advance_epoch(next) {
                                send_results.send(Output::Error(client, error, last_tx)).unwrap();
                            }
                        }
                        Request::Status => {
                            let status = serde_json::json!({
                                "category": "df/status",
                                "message": "running",
                            });

                            send_results.send(Output::Message(client, status)).unwrap();
                        }
                        Request::Shutdown => {
                            shutdown = true
                        }
                    }
                }

                if !config.manual_advance {
                    #[cfg(all(not(feature = "real-time"), not(feature = "bitemporal")))]
                    let next = next_tx as u64;
                    #[cfg(feature = "real-time")]
                    let next = Instant::now().duration_since(worker.timer());
                    #[cfg(feature = "bitemporal")]
                    let next = Pair::new(Instant::now().duration_since(worker.timer()), next_tx as u64);

                    server.context.internal.advance_epoch(next).expect("failed to advance epoch");
                }
            }

            // We must always ensure that workers step in every
            // iteration, even if no queries registered, s.t. the
            // sequencer can continue propagating commands. We also
            // want to limit the maximal number of steps here to avoid
            // stalling user inputs.
            for _i in 0..32 {
                worker.step();
            }

            // We advance before `step_or_park`, because advancing
            // might take a decent amount of time, in case traces get
            // compacted. If that happens, we can park less before
            // scheduling the next activator.
            server.context.internal.advance().expect("failed to advance domain");

            // Finally, we give the CPU a chance to chill, if no work
            // remains.
            let delay = server.scheduler.borrow().until_next().unwrap_or(Duration::from_millis(100));
            worker.step_or_park(Some(delay));
        }

        info!("[WORKER {}] shutting down", worker.index());

        drop(sequencer);

        // Shutdown loggers s.t. logging dataflows can shut down.
        #[cfg(feature = "real-time")]
        server.shutdown_logging(worker).unwrap();

    }).expect("Timely computation did not exit cleanly");
}
