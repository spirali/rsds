use std::cmp::Reverse;

use bytes::{Bytes, BytesMut};
use hashbrown::HashMap;
use tokio::sync::mpsc::UnboundedSender;

use crate::common::data::SerializationType;
use crate::common::{Map, WrappedRcRefCell};
use crate::scheduler::TaskId;
use crate::server::protocol::messages::worker::{
    DataDownloadedMsg, FromWorkerMessage, StealResponse,
};
use crate::server::protocol::{Priority, PriorityValue};
use crate::server::worker::WorkerId;
use crate::worker::data::{DataObjectRef, DataObjectState, LocalData, RemoteData};
use crate::worker::reactor::choose_subworker;
use crate::worker::subworker::{SubworkerId, SubworkerRef};
use crate::worker::task::{TaskRef, TaskState};

pub type WorkerStateRef = WrappedRcRefCell<WorkerState>;

pub struct WorkerState {
    pub sender: UnboundedSender<Bytes>,
    pub ncpus: u32,
    pub listen_address: String,
    pub subworkers: HashMap<SubworkerId, SubworkerRef>,
    pub free_subworkers: Vec<SubworkerRef>,
    pub tasks: HashMap<TaskId, TaskRef>,
    pub ready_task_queue:
        priority_queue::PriorityQueue<TaskRef, Reverse<(PriorityValue, PriorityValue)>>,
    pub data_objects: HashMap<TaskId, DataObjectRef>,
    pub download_sender: tokio::sync::mpsc::UnboundedSender<(DataObjectRef, Priority)>,
    pub worker_id: WorkerId,
    pub worker_addresses: Map<WorkerId, String>,
}

impl WorkerState {
    pub fn set_subworkers(&mut self, subworkers: Vec<SubworkerRef>) {
        assert!(self.subworkers.is_empty() && self.free_subworkers.is_empty());
        self.free_subworkers = subworkers.clone();
        self.subworkers = subworkers
            .iter()
            .map(|s| {
                let id = s.get().id;
                (id, s.clone())
            })
            .collect();
    }

    pub fn add_data_object(&mut self, data_ref: DataObjectRef) {
        let id = data_ref.get().id;
        self.data_objects.insert(id, data_ref);
    }

    pub fn send_message_to_server(&self, data: Vec<u8>) {
        self.sender.send(data.into()).unwrap();
    }

    pub fn on_data_downloaded(
        &mut self,
        data_ref: DataObjectRef,
        data: BytesMut,
        serializer: SerializationType,
    ) {
        {
            let mut data_obj = data_ref.get_mut();
            log::debug!("Data {} downloaded ({} bytes)", data_obj.id, data.len());
            match data_obj.state {
                DataObjectState::Remote(_) => { /* This is ok */ }
                DataObjectState::Removed => {
                    /* download was completed, but we do not care about data */
                    log::debug!("Data is not needed any more");
                    return;
                }
                DataObjectState::Local(_) => unreachable!(),
            }
            data_obj.state = DataObjectState::Local(LocalData {
                serializer,
                bytes: data.into(),
            });

            let message = FromWorkerMessage::DataDownloaded(DataDownloadedMsg { id: data_obj.id });
            self.send_message_to_server(rmp_serde::to_vec_named(&message).unwrap());
        }

        /* TODO: Inform server about download */

        /* We need to reborrow the data_ref to readonly as
          add_ready_task may start to read this data_ref
        */
        for task_ref in &data_ref.get().consumers {
            let is_ready = task_ref.get_mut().decrease_waiting_count();
            if is_ready {
                log::debug!("Task {} becomes ready", task_ref.get().id);
                self.add_ready_task(task_ref.clone());
            }
        }
    }

    pub fn add_ready_task(&mut self, task_ref: TaskRef) {
        let priority = task_ref.get().priority.clone();
        self.ready_task_queue.push(task_ref, Reverse(priority));
        self.try_start_tasks();
    }

    pub fn add_dependancy(
        &mut self,
        task_ref: &TaskRef,
        task_id: TaskId,
        size: u64,
        workers: Vec<WorkerId>,
    ) {
        let mut task = task_ref.get_mut();
        let mut is_remote = false;
        let data_ref = match self.data_objects.get(&task_id).cloned() {
            None => {
                let data_ref = DataObjectRef::new(
                    task_id,
                    size,
                    DataObjectState::Remote(RemoteData { workers }),
                );
                self.data_objects.insert(task_id, data_ref.clone());
                is_remote = true;
                data_ref
            }
            Some(data_ref) => {
                {
                    let mut data_obj = data_ref.get_mut();
                    match data_obj.state {
                        DataObjectState::Remote(_) => {
                            is_remote = true;
                            data_obj.state = DataObjectState::Remote(RemoteData { workers })
                        }
                        DataObjectState::Local(_) => { /* Do nothing */ }
                        DataObjectState::Removed => {
                            unreachable!();
                        }
                    };
                }
                data_ref
            }
        };
        data_ref.get_mut().consumers.insert(task_ref.clone());
        if is_remote {
            task.increase_waiting_count();
            let _ = self.download_sender.send((data_ref.clone(), task.priority));
        }
        task.deps.push(data_ref);
    }

    pub fn add_task(&mut self, task_ref: TaskRef) {
        let id = task_ref.get().id;
        if task_ref.get().is_ready() {
            log::debug!("Task {} is directly ready", id);
            self.add_ready_task(task_ref.clone());
        } else {
            let task = task_ref.get();
            log::debug!(
                "Task {} is blocked by {} remote objects",
                id,
                task.get_waiting()
            );
        }
        self.tasks.insert(id, task_ref);
    }

    pub fn try_start_tasks(&mut self) {
        if self.free_subworkers.is_empty() {
            return;
        }
        while let Some((task_ref, _)) = self.ready_task_queue.pop() {
            {
                let subworker_ref = choose_subworker(self);
                let mut task = task_ref.get_mut();
                task.set_running(subworker_ref.clone());
                let mut sw = subworker_ref.get_mut();
                assert!(sw.running_task.is_none());
                sw.running_task = Some(task_ref.clone());
                sw.start_task(&task);
            }
            if self.free_subworkers.is_empty() {
                return;
            }
        }
    }

    pub fn remove_data(&mut self, task_id: TaskId) {
        log::info!("Removing data object {}", task_id);
        self.data_objects.remove(&task_id).map(|data_ref| {
            let mut data_obj = data_ref.get_mut();
            data_obj.state = DataObjectState::Removed;
            if !data_obj.consumers.is_empty() {
                todo!(); // What should happen when server removes data but there are tasks that needs it?
            }
        });
    }

    pub fn remove_task(&mut self, task_ref: TaskRef, just_finished: bool) {
        let mut task = task_ref.get_mut();
        match task.state {
            TaskState::Waiting(x) => {
                assert!(!just_finished);
                if x == 0 {
                    assert!(self.ready_task_queue.remove(&task_ref).is_some());
                }
            }
            TaskState::Running(_) => {
                assert!(just_finished);
            }
            TaskState::Removed => {
                unreachable!();
            }
        }
        task.state = TaskState::Removed;

        assert!(self.tasks.remove(&task.id).is_some());

        for data_ref in std::mem::take(&mut task.deps) {
            let mut data = data_ref.get_mut();
            assert!(data.consumers.remove(&task_ref));
            if data.consumers.is_empty() {
                match data.state {
                    DataObjectState::Remote(_) => {
                        assert!(!just_finished);
                        data.state = DataObjectState::Removed;
                    }
                    DataObjectState::Local(_) => { /* Do nothing */ }
                    DataObjectState::Removed => { /* Do nothing */ }
                };
            }
        }
    }

    pub fn steal_task(&mut self, task_id: TaskId) -> StealResponse {
        match self.tasks.get(&task_id).cloned() {
            None => StealResponse::NotHere,
            Some(task_ref) => {
                {
                    let task = task_ref.get_mut();
                    match task.state {
                        TaskState::Waiting(_) => { /* Continue */ }
                        TaskState::Running(_) => return StealResponse::Running,
                        TaskState::Removed => return StealResponse::NotHere,
                    }
                }
                self.remove_task(task_ref, false);
                StealResponse::Ok
            }
        }
    }
}

impl WorkerStateRef {
    pub fn new(
        worker_id: WorkerId,
        sender: UnboundedSender<Bytes>,
        ncpus: u32,
        listen_address: String,
        download_sender: tokio::sync::mpsc::UnboundedSender<(DataObjectRef, Priority)>,
        worker_addresses: Map<WorkerId, String>,
    ) -> Self {
        Self::wrap(WorkerState {
            worker_id,
            worker_addresses,
            sender,
            ncpus,
            listen_address,
            download_sender,
            tasks: Default::default(),
            subworkers: Default::default(),
            free_subworkers: Default::default(),
            ready_task_queue: Default::default(),
            data_objects: Default::default(),
        })
    }
}
