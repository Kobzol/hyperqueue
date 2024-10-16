use std::rc::Rc;
use std::sync::Arc;

use futures::{Sink, SinkExt, Stream, StreamExt};
use orion::kdf::SecretKey;
use tako::messages::common::{ProgramDefinition, TaskConfiguration};
use tako::messages::gateway::{
    CancelTasks, FromGatewayMessage, GenericResourceNames, NewTasksMessage, OverviewRequest,
    StopWorkerRequest, TaskDef, ToGatewayMessage,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};

use crate::client::status::{job_status, task_status, Status};
use crate::common::arraydef::IntArray;
use crate::common::env::{HQ_ENTRY, HQ_JOB_ID, HQ_SUBMIT_DIR, HQ_TASK_ID};
use crate::common::manager::info::ManagerType;
use crate::common::serverdir::ServerDir;

use crate::server::autoalloc::{
    DescriptorId, PbsHandler, QueueDescriptor, QueueHandler, QueueInfo, SlurmHandler,
};
use crate::server::job::{Job, JobState, JobTaskCounters};
use crate::server::rpc::Backend;
use crate::server::state::{State, StateRef};
use crate::stream::server::control::StreamServerControlMessage;
use crate::transfer::connection::ServerConnection;
use crate::transfer::messages::{
    AddQueueRequest, AutoAllocListResponse, AutoAllocRequest, AutoAllocResponse, CancelJobResponse,
    FromClientMessage, JobDetail, JobInfoResponse, JobType, QueueDescriptorData, ResubmitRequest,
    Selector, StatsResponse, StopWorkerResponse, SubmitRequest, SubmitResponse, TaskBody,
    ToClientMessage, WorkerListResponse,
};
use crate::{JobId, JobTaskCount, JobTaskId, WorkerId};
use bstr::BString;

use crate::common::placeholders::replace_placeholders_server;
use crate::transfer::messages::WaitForJobsResponse;
use std::path::Path;

pub async fn handle_client_connections(
    state_ref: StateRef,
    tako_ref: Backend,
    server_dir: ServerDir,
    listener: TcpListener,
    end_flag: Rc<Notify>,
    key: Arc<SecretKey>,
) {
    while let Ok((connection, _)) = listener.accept().await {
        let state_ref = state_ref.clone();
        let tako_ref = tako_ref.clone();
        let end_flag = end_flag.clone();
        let key = key.clone();
        let server_dir = server_dir.clone();

        tokio::task::spawn_local(async move {
            if let Err(e) =
                handle_client(connection, server_dir, state_ref, tako_ref, end_flag, key).await
            {
                log::error!("Client error: {}", e);
            }
        });
    }
}

async fn handle_client(
    socket: TcpStream,
    server_dir: ServerDir,
    state_ref: StateRef,
    tako_ref: Backend,
    end_flag: Rc<Notify>,
    key: Arc<SecretKey>,
) -> crate::Result<()> {
    log::debug!("New client connection");
    let socket = ServerConnection::accept_client(socket, key).await?;
    let (tx, rx) = socket.split();

    client_rpc_loop(tx, rx, server_dir, state_ref, tako_ref, end_flag).await;
    log::debug!("Client connection ended");
    Ok(())
}

pub async fn client_rpc_loop<
    Tx: Sink<ToClientMessage> + Unpin,
    Rx: Stream<Item = crate::Result<FromClientMessage>> + Unpin,
>(
    mut tx: Tx,
    mut rx: Rx,
    server_dir: ServerDir,
    state_ref: StateRef,
    tako_ref: Backend,
    end_flag: Rc<Notify>,
) {
    while let Some(message_result) = rx.next().await {
        match message_result {
            Ok(message) => {
                let response = match message {
                    FromClientMessage::Submit(msg) => {
                        handle_submit(&state_ref, &tako_ref, msg).await
                    }
                    FromClientMessage::JobInfo(msg) => compute_job_info(&state_ref, msg.selector),
                    FromClientMessage::Resubmit(msg) => {
                        handle_resubmit(&state_ref, &tako_ref, msg).await
                    }
                    FromClientMessage::Stop => {
                        end_flag.notify_one();
                        break;
                    }
                    FromClientMessage::WorkerList => handle_worker_list(&state_ref).await,
                    FromClientMessage::WorkerInfo(msg) => {
                        handle_worker_info(&state_ref, msg.worker_id).await
                    }
                    FromClientMessage::StopWorker(msg) => {
                        handle_worker_stop(&state_ref, &tako_ref, msg.selector).await
                    }
                    FromClientMessage::Cancel(msg) => {
                        handle_job_cancel(&state_ref, &tako_ref, msg.selector).await
                    }
                    FromClientMessage::JobDetail(msg) => {
                        compute_job_detail(&state_ref, msg.selector, msg.include_tasks)
                    }
                    FromClientMessage::Stats => compose_server_stats(&state_ref, &tako_ref).await,
                    FromClientMessage::AutoAlloc(msg) => {
                        handle_autoalloc_message(&server_dir, &state_ref, msg).await
                    }
                    FromClientMessage::WaitForJobs(msg) => {
                        handle_wait_for_jobs_message(&state_ref, msg.selector).await
                    }
                    FromClientMessage::Overview(_overview_request) => {
                        //todo: take in overview_request as param
                        get_collected_overview(&tako_ref).await
                    }
                    FromClientMessage::GetGenericResourceNames(msg) => {
                        handle_get_resource_ids(&tako_ref, msg).await
                    }
                };
                assert!(tx.send(response).await.is_ok());
            }
            Err(e) => {
                log::error!("Cannot parse client message: {}", e);
                assert!(tx
                    .send(ToClientMessage::Error(format!(
                        "Cannot parse message: {}",
                        e
                    )))
                    .await
                    .is_ok());
                return;
            }
        }
    }
}

/// Waits until all jobs matched by the `selector` are finished (either by completing successfully,
/// failing or being canceled).
async fn handle_wait_for_jobs_message(state_ref: &StateRef, selector: Selector) -> ToClientMessage {
    let update_counters = |response: &mut WaitForJobsResponse, counters: &JobTaskCounters| {
        if counters.n_canceled_tasks > 0 {
            response.canceled += 1;
        } else if counters.n_failed_tasks > 0 {
            response.failed += 1;
        } else {
            response.finished += 1;
        }
    };

    let (receivers, mut response) = {
        let mut state = state_ref.get_mut();
        let job_ids: Vec<JobId> = get_job_ids(&state, selector);

        let mut response = WaitForJobsResponse::default();
        let mut receivers = vec![];

        for job_id in job_ids {
            match state.get_job_mut(job_id) {
                Some(job) => {
                    if job.is_terminated() {
                        update_counters(&mut response, &job.counters);
                    } else {
                        let rx = job.subscribe_to_completion();
                        receivers.push(rx);
                    }
                }
                None => response.invalid += 1,
            }
        }
        (receivers, response)
    };

    let results = futures::future::join_all(receivers).await;
    let state = state_ref.get();

    for result in results {
        match result {
            Ok(job_id) => {
                match state.get_job(job_id) {
                    Some(job) => update_counters(&mut response, &job.counters),
                    None => continue,
                };
            }
            Err(err) => log::error!("Error while waiting on job(s): {:?}", err),
        };
    }

    ToClientMessage::WaitForJobsResponse(response)
}

async fn handle_autoalloc_message(
    server_dir: &ServerDir,
    state_ref: &StateRef,
    request: AutoAllocRequest,
) -> ToClientMessage {
    match request {
        AutoAllocRequest::List => {
            let state = state_ref.get();
            let autoalloc = state.get_autoalloc_state();
            ToClientMessage::AutoAllocResponse(AutoAllocResponse::List(AutoAllocListResponse {
                descriptors: autoalloc
                    .descriptors()
                    .map(|(id, descriptor)| {
                        (
                            id,
                            QueueDescriptorData {
                                info: descriptor.descriptor.info().clone(),
                                name: descriptor.descriptor.name().map(|v| v.to_string()),
                                manager_type: descriptor.descriptor.manager().clone(),
                            },
                        )
                    })
                    .collect(),
            }))
        }
        AutoAllocRequest::AddQueue(params) => create_queue(server_dir, state_ref, params),
        AutoAllocRequest::Events { descriptor } => get_event_log(state_ref, descriptor),
        AutoAllocRequest::Info { descriptor } => get_allocations(state_ref, descriptor),
        AutoAllocRequest::RemoveQueue(id) => remove_queue(state_ref, id).await,
    }
}

async fn handle_get_resource_ids(
    tako_ref: &Backend,
    message: GenericResourceNames,
) -> ToClientMessage {
    let response = tako_ref
        .send_tako_message(FromGatewayMessage::GetGenericResourceNames(message))
        .await
        .unwrap();
    if let ToGatewayMessage::GenericResourceNames(r) = response {
        ToClientMessage::GenericResourceNames(r)
    } else {
        ToClientMessage::Error("Failed to resource ids".to_string())
    }
}

async fn remove_queue(state_ref: &StateRef, id: DescriptorId) -> ToClientMessage {
    let remove_alloc_fut = {
        let mut server_state = state_ref.get_mut();
        let descriptor_state = server_state
            .get_autoalloc_state_mut()
            .get_descriptor_mut(id);

        let fut = match descriptor_state {
            Some(state) => {
                let handler = state.descriptor.handler();
                let futures: Vec<_> = state
                    .active_allocations()
                    .map(move |alloc| {
                        let id = alloc.id.clone();
                        let future = handler.remove_allocation(alloc.id.clone());
                        async move { (future.await, id) }
                    })
                    .collect();
                futures
            }
            None => return ToClientMessage::Error("Descriptor not found".to_string()),
        };

        server_state.get_autoalloc_state_mut().remove_descriptor(id);
        fut
    };

    for (result, allocation_id) in futures::future::join_all(remove_alloc_fut).await {
        match result {
            Ok(_) => log::info!("Allocation {} was removed", allocation_id),
            Err(e) => log::error!("Failed to remove allocation {}: {:?}", allocation_id, e),
        }
    }

    ToClientMessage::AutoAllocResponse(AutoAllocResponse::QueueRemoved(id))
}

fn get_allocations(state_ref: &StateRef, descriptor: DescriptorId) -> ToClientMessage {
    let state = state_ref.get();
    let autoalloc = state.get_autoalloc_state();

    match autoalloc.get_descriptor(descriptor) {
        Some(descriptor) => ToClientMessage::AutoAllocResponse(AutoAllocResponse::Info(
            descriptor.all_allocations().cloned().collect(),
        )),
        None => ToClientMessage::Error(format!("Descriptor {} not found", descriptor)),
    }
}

fn get_event_log(state_ref: &StateRef, descriptor: DescriptorId) -> ToClientMessage {
    let state = state_ref.get();
    let autoalloc = state.get_autoalloc_state();

    match autoalloc.get_descriptor(descriptor) {
        Some(descriptor) => ToClientMessage::AutoAllocResponse(AutoAllocResponse::Events(
            descriptor.get_events().iter().cloned().collect(),
        )),
        None => ToClientMessage::Error(format!("Descriptor {} not found", descriptor)),
    }
}

fn create_queue(
    server_dir: &ServerDir,
    state_ref: &StateRef,
    request: AddQueueRequest,
) -> ToClientMessage {
    let server_directory = server_dir.directory().to_path_buf();
    let (handler, params, manager_type) = match request {
        AddQueueRequest::Pbs(params) => {
            let handler = PbsHandler::new(server_directory, params.name.clone());
            (
                handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler)),
                params,
                ManagerType::Pbs,
            )
        }
        AddQueueRequest::Slurm(params) => {
            let handler = SlurmHandler::new(server_directory, params.name.clone());
            (
                handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler)),
                params,
                ManagerType::Slurm,
            )
        }
    };

    let queue_info = QueueInfo::new(
        params.backlog,
        params.workers_per_alloc,
        params.timelimit,
        params.additional_args,
    );

    match handler {
        Ok(handler) => {
            let descriptor = QueueDescriptor::new(manager_type, queue_info, params.name, handler);
            let id = state_ref.get_mut().get_autoalloc_state_mut().create_id();
            state_ref
                .get_mut()
                .get_autoalloc_state_mut()
                .add_descriptor(id, descriptor);
            ToClientMessage::AutoAllocResponse(AutoAllocResponse::QueueCreated(id))
        }
        Err(err) => ToClientMessage::Error(format!("Could not create autoalloc queue: {}", err)),
    }
}

async fn handle_worker_stop(
    state_ref: &StateRef,
    tako_ref: &Backend,
    selector: Selector,
) -> ToClientMessage {
    log::debug!("Client asked for worker termination {:?}", selector);
    let mut responses: Vec<(WorkerId, StopWorkerResponse)> = Vec::new();

    let worker_ids: Vec<WorkerId> = match selector {
        Selector::Specific(array) => array.iter().collect(),
        Selector::All => state_ref
            .get()
            .get_workers()
            .iter()
            .filter(|(_, worker)| worker.make_info().ended.is_none())
            .map(|(_, worker)| worker.worker_id())
            .collect(),
        Selector::LastN(n) => {
            let mut ids: Vec<_> = state_ref.get().get_workers().keys().copied().collect();
            ids.sort_by_key(|&k| std::cmp::Reverse(k));
            ids.truncate(n as usize);
            ids
        }
    };

    for worker_id in worker_ids {
        if let Some(worker) = state_ref.get().get_worker(worker_id) {
            if worker.make_info().ended.is_some() {
                responses.push((worker_id, StopWorkerResponse::AlreadyStopped));
                continue;
            }
        } else {
            responses.push((worker_id, StopWorkerResponse::InvalidWorker));
            continue;
        }
        let response = tako_ref
            .clone()
            .send_tako_message(FromGatewayMessage::StopWorker(StopWorkerRequest {
                worker_id,
            }))
            .await;

        match response {
            Ok(result) => match result {
                ToGatewayMessage::WorkerStopped => {
                    responses.push((worker_id, StopWorkerResponse::Stopped))
                }
                ToGatewayMessage::Error(error) => {
                    responses.push((worker_id, StopWorkerResponse::Failed(error.message)))
                }
                msg => panic!(
                    "Received invalid response to worker: {} stop: {:?}",
                    worker_id, msg
                ),
            },
            Err(err) => {
                responses.push((worker_id, StopWorkerResponse::Failed(err.to_string())));
                log::error!("Unable to stop worker: {} error: {:?}", worker_id, err);
            }
        }
    }
    ToClientMessage::StopWorkerResponse(responses)
}

fn compute_job_detail(
    state_ref: &StateRef,
    selector: Selector,
    include_tasks: bool,
) -> ToClientMessage {
    let state = state_ref.get();

    let job_ids: Vec<JobId> = get_job_ids(&state, selector);

    let mut responses: Vec<(JobId, Option<JobDetail>)> = Vec::new();
    for job_id in job_ids {
        let opt_detail = state
            .get_job(job_id)
            .map(|j| j.make_job_detail(include_tasks));

        if let Some(detail) = opt_detail {
            responses.push((job_id, Some(detail)));
        } else {
            responses.push((job_id, None));
        }
    }
    ToClientMessage::JobDetailResponse(responses)
}

fn get_job_ids(state: &State, selector: Selector) -> Vec<JobId> {
    match selector {
        Selector::All => state.jobs().map(|job| job.job_id).collect(),
        Selector::LastN(n) => state.last_n_ids(n).collect(),
        Selector::Specific(array) => array.iter().collect(),
    }
}

async fn compose_server_stats(_state_ref: &StateRef, backend: &Backend) -> ToClientMessage {
    let stream_stats = {
        let (sender, receiver) = oneshot::channel();
        backend.send_stream_control(StreamServerControlMessage::Stats(sender));
        receiver.await.unwrap()
    };
    ToClientMessage::StatsResponse(StatsResponse { stream_stats })
}

fn compute_job_info(state_ref: &StateRef, selector: Selector) -> ToClientMessage {
    let state = state_ref.get();

    let jobs: Vec<_> = match selector {
        Selector::All => state.jobs().map(|j| j.make_job_info()).collect(),
        Selector::LastN(n) => state
            .last_n_ids(n)
            .filter_map(|id| state.get_job(id))
            .map(|j| j.make_job_info())
            .collect(),
        Selector::Specific(array) => array
            .iter()
            .filter_map(|id| state.get_job(id))
            .map(|j| j.make_job_info())
            .collect(),
    };
    ToClientMessage::JobInfoResponse(JobInfoResponse { jobs })
}

async fn handle_job_cancel(
    state_ref: &StateRef,
    tako_ref: &Backend,
    selector: Selector,
) -> ToClientMessage {
    let job_ids: Vec<JobId> = match selector {
        Selector::All => state_ref
            .get()
            .jobs()
            .map(|job| job.make_job_info())
            .filter(|job_info| matches!(job_status(job_info), Status::Waiting | Status::Running))
            .map(|job_info| job_info.id)
            .collect(),
        Selector::LastN(n) => state_ref.get().last_n_ids(n).collect(),
        Selector::Specific(array) => array.iter().collect(),
    };

    let mut responses: Vec<(JobId, CancelJobResponse)> = Vec::new();
    for job_id in job_ids {
        let tako_task_ids;
        {
            let n_tasks = match state_ref.get().get_job(job_id) {
                None => {
                    responses.push((job_id, CancelJobResponse::InvalidJob));
                    continue;
                }
                Some(job) => {
                    tako_task_ids = job.non_finished_task_ids();
                    job.n_tasks()
                }
            };
            if tako_task_ids.is_empty() {
                responses.push((job_id, CancelJobResponse::Canceled(Vec::new(), n_tasks)));
                continue;
            }
        }

        let canceled_tasks = match tako_ref
            .send_tako_message(FromGatewayMessage::CancelTasks(CancelTasks {
                tasks: tako_task_ids,
            }))
            .await
            .unwrap()
        {
            ToGatewayMessage::CancelTasksResponse(msg) => msg.cancelled_tasks,
            ToGatewayMessage::Error(msg) => {
                responses.push((job_id, CancelJobResponse::Failed(msg.message)));
                continue;
            }
            _ => panic!("Invalid message"),
        };

        let mut state = state_ref.get_mut();
        let job = state.get_job_mut(job_id).unwrap();
        let canceled_ids: Vec<_> = canceled_tasks
            .iter()
            .map(|tako_id| job.set_cancel_state(*tako_id, tako_ref))
            .collect();
        let already_finished = job.n_tasks() - canceled_ids.len() as JobTaskCount;
        responses.push((
            job_id,
            CancelJobResponse::Canceled(canceled_ids, already_finished),
        ));
    }

    ToClientMessage::CancelJobResponse(responses)
}

fn make_program_def_for_task(
    program_def: &ProgramDefinition,
    job_id: JobId,
    task_id: JobTaskId,
    submit_dir: &Path,
) -> ProgramDefinition {
    let mut def = program_def.clone();
    def.env.insert(HQ_JOB_ID.into(), job_id.to_string().into());
    def.env
        .insert(HQ_TASK_ID.into(), task_id.to_string().into());
    def.env.insert(
        HQ_SUBMIT_DIR.into(),
        BString::from(submit_dir.to_string_lossy().as_bytes()),
    );
    def
}

async fn handle_submit(
    state_ref: &StateRef,
    tako_ref: &Backend,
    message: SubmitRequest,
) -> ToClientMessage {
    if message.resources.validate().is_err() {
        return ToClientMessage::Error("Invalid resource request".to_string());
    }
    let resources = message.resources;
    let spec = message.spec;
    let pin = message.pin;
    let submit_dir = message.submit_dir;
    let priority = message.priority;
    let time_limit = message.time_limit;

    let make_task = |job_id, task_id, tako_id, entry: Option<BString>| {
        let mut program = make_program_def_for_task(&spec, job_id, task_id, &submit_dir);
        if let Some(e) = entry {
            program.env.insert(HQ_ENTRY.into(), e);
        }
        let body_msg = TaskBody {
            program,
            pin,
            job_id,
            task_id,
        };
        let body = tako::transfer::auth::serialize(&body_msg).unwrap();
        TaskDef {
            id: tako_id,
            conf: TaskConfiguration {
                resources: resources.clone(),
                n_outputs: 0,
                time_limit,
                body,
            },
            keep: false,
            observe: true,
            priority,
        }
    };
    let (task_defs, job_detail, job_id) = {
        let mut state = state_ref.get_mut();
        let job_id = state.new_job_id();
        let task_count = match &message.job_type {
            JobType::Simple => 1,
            JobType::Array(a) => a.id_count(),
        };
        let tako_base_id = state.new_task_id(task_count);
        let task_defs = match (&message.job_type, message.entries.clone()) {
            (JobType::Simple, _) => vec![make_task(job_id, 0, tako_base_id, None)],
            (JobType::Array(a), None) => a
                .iter()
                .zip(tako_base_id..)
                .map(|(task_id, tako_id)| make_task(job_id, task_id, tako_id, None))
                .collect(),
            (JobType::Array(a), Some(entries)) => a
                .iter()
                .zip(tako_base_id..)
                .zip(entries.into_iter())
                .map(|((task_id, tako_id), entry)| make_task(job_id, task_id, tako_id, Some(entry)))
                .collect(),
        };
        let job = Job::new(
            message.job_type,
            job_id,
            tako_base_id,
            message.name.clone(),
            spec,
            resources,
            pin,
            message.max_fails,
            message.entries.clone(),
            priority,
            time_limit,
            message.log.clone(),
        );
        let job_detail = job.make_job_detail(false);
        state.add_job(job);

        (task_defs, job_detail, job_id)
    };

    if let Some(mut log) = message.log {
        replace_placeholders_server(&mut log, job_id, submit_dir.clone());

        let (sender, receiver) = oneshot::channel();
        tako_ref.send_stream_control(StreamServerControlMessage::RegisterStream {
            job_id,
            path: submit_dir.join(log),
            response: sender,
        });
        assert!(receiver.await.is_ok());
    }

    match tako_ref
        .send_tako_message(FromGatewayMessage::NewTasks(NewTasksMessage {
            tasks: task_defs,
        }))
        .await
        .unwrap()
    {
        ToGatewayMessage::NewTasksResponse(_) => { /* Ok */ }
        _ => {
            panic!("Invalid response");
        }
    };

    ToClientMessage::SubmitResponse(SubmitResponse { job: job_detail })
}

async fn handle_resubmit(
    state_ref: &StateRef,
    tako_ref: &Backend,
    message: ResubmitRequest,
) -> ToClientMessage {
    let msg_submit: SubmitRequest = {
        let state = state_ref.get_mut();
        let job = state.get_job(message.job_id);
        if let Some(job) = job {
            let job_type: Option<JobType> = match &job.state {
                JobState::SingleTask(s) => {
                    if let Some(filter) = &message.status {
                        if filter.contains(&task_status(s)) {
                            Some(JobType::Simple)
                        } else {
                            None
                        }
                    } else {
                        Some(JobType::Simple)
                    }
                }
                JobState::ManyTasks(s) => {
                    let mut ids: Vec<_> = if let Some(filter) = &message.status {
                        s.values()
                            .filter_map(|v| {
                                if filter.contains(&task_status(&v.state)) {
                                    Some(v.task_id)
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        s.values().map(|x| x.task_id).collect()
                    };
                    if ids.is_empty() {
                        None
                    } else {
                        ids.sort_unstable();
                        Some(JobType::Array(IntArray::from_ids(ids)))
                    }
                }
            };

            if let Some(job_type) = job_type {
                let spec = job.program_def.clone();
                let name = job.name.clone();
                let resources = job.resources.clone();
                let entries = job.entries.clone();

                SubmitRequest {
                    job_type,
                    name,
                    max_fails: job.max_fails,
                    spec,
                    resources,
                    pin: job.pin,
                    entries,
                    submit_dir: std::env::current_dir().unwrap().to_str().unwrap().into(),
                    priority: job.priority,
                    time_limit: job.time_limit,
                    log: None, // TODO: Reuse log configuration
                }
            } else {
                return ToClientMessage::Error("Nothing was resubmitted".to_string());
            }
        } else {
            return ToClientMessage::Error("Invalid job_id".to_string());
        }
    };
    handle_submit(&state_ref.clone(), &tako_ref.clone(), msg_submit).await
}

async fn handle_worker_list(state_ref: &StateRef) -> ToClientMessage {
    let state = state_ref.get();

    ToClientMessage::WorkerListResponse(WorkerListResponse {
        workers: state
            .get_workers()
            .values()
            .map(|w| w.make_info())
            .collect(),
    })
}

async fn handle_worker_info(state_ref: &StateRef, worker_id: WorkerId) -> ToClientMessage {
    let state = state_ref.get();

    ToClientMessage::WorkerInfoResponse(state.get_worker(worker_id).map(|w| w.make_info()))
}

async fn get_collected_overview(tako_ref: &Backend) -> ToClientMessage {
    let response = tako_ref
        .send_tako_message(FromGatewayMessage::GetOverview(OverviewRequest {
            enable_hw_overview: true,
        }))
        .await
        .unwrap();
    //todo: if let
    match response {
        ToGatewayMessage::Overview(collected_overview) => {
            ToClientMessage::OverviewResponse(collected_overview)
        }
        _ => ToClientMessage::Error("overview request failed".to_string()),
    }
}
