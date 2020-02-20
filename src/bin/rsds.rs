use std::net::{Ipv4Addr, SocketAddr};
use std::thread;

use futures::{FutureExt, StreamExt};
use structopt::StructOpt;
use tokio::net::TcpListener;

use rsds::comm::{observe_scheduler, CommRef};
use rsds::core::CoreRef;
use rsds::scheduler::comm::{prepare_scheduler_comm, SchedulerComm};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn create_scheduler(
    r#type: SchedulerType,
    comm: SchedulerComm,
) -> Pin<Box<dyn Future<Output = rsds::Result<()>>>> {
    match r#type {
        SchedulerType::Workstealing => {
            Box::pin(rsds::scheduler::workstealing::Scheduler::new().start(comm))
        }
        SchedulerType::Random => Box::pin(rsds::scheduler::random::Scheduler::new().start(comm)),
    }
}

#[derive(Debug)]
enum SchedulerType {
    Workstealing,
    Random,
}

impl FromStr for SchedulerType {
    type Err = String;
    fn from_str(scheduler: &str) -> Result<Self, Self::Err> {
        match scheduler {
            "workstealing" => Ok(SchedulerType::Workstealing),
            "random" => Ok(SchedulerType::Random),
            _ => Err(format!("Scheduler '{}' does not exist", scheduler)),
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "rsds", about = "Rust Dask Scheduler")]
struct Opt {
    #[structopt(long, default_value = "8786")]
    port: u16,
    #[structopt(long, default_value = "workstealing")]
    scheduler: SchedulerType,
}

#[tokio::main(basic_scheduler)]
async fn main() -> rsds::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init();

    let (end_tx, mut end_rx) = tokio::sync::mpsc::unbounded_channel();
    ctrlc::set_handler(move || {
        log::info!("Received SIGINT, attempting to stop server");
        end_tx.send(()).unwrap_or_else(|_| log::error!("Sending"))
    })
    .expect("Error setting Ctrl-C handler");

    let opt = Opt::from_args();

    log::info!("rsds v0.1 started: {:?}", opt);

    let address = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), opt.port);
    log::info!("listening on port {}", address);
    let listener = TcpListener::bind(address).await?;

    let (comm, sender, receiver) = prepare_scheduler_comm();

    let scheduler_thread = thread::spawn(move || {
        let mut runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .build()
            .expect("Runtime creation failed");
        runtime
            .block_on(create_scheduler(opt.scheduler, comm))
            .expect("Scheduler failed");
    });

    {
        let task_set = tokio::task::LocalSet::new();
        let comm_ref = CommRef::new(sender);
        let core_ref = CoreRef::new();
        let core_ref2 = core_ref.clone();
        let comm_ref2 = comm_ref.clone();
        task_set
            .run_until(async move {
                let scheduler = observe_scheduler(core_ref2, comm_ref2, receiver);
                let connection =
                    rsds::comm::connection_initiator(listener, core_ref, comm_ref);
                let end_flag = async move {
                    end_rx.next().await;
                    Ok(())
                };

                let futures = vec![scheduler.boxed_local(), connection.boxed_local(), end_flag.boxed_local()];
                let (res, _, _) = futures::future::select_all(futures).await;
                res
            })
            .await
            .expect("Rsds failed");
    }

    log::info!("Waiting for scheduler to shut down...");
    scheduler_thread.join().unwrap();

    Ok(())
}
