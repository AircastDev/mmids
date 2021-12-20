use crate::event_hub::{SubscriptionRequest, WorkflowManagerEvent};
use crate::reactors::executors::ReactorExecutor;
use crate::workflows::definitions::WorkflowDefinition;
use crate::workflows::manager::{WorkflowManagerRequest, WorkflowManagerRequestOperation};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, instrument};

/// Requests that can be made to a reactor
pub enum ReactorRequest {
    /// Requests that the reactor creates and manages a workflow for the specified stream name
    CreateWorkflowNameForStream {
        /// Name of the stream to get a workflow for
        stream_name: String,

        /// The channel to send a response for. This channel will not only be used for the
        /// initial response, but updates will be sent any time the reactor detects changes.
        response_channel: UnboundedSender<ReactorWorkflowUpdate>,
    },
}

/// Contains information about a workflow from a reactor
pub struct ReactorWorkflowUpdate {
    /// The name of the current workflow the consumer should utilize. If `None` is returned then
    /// that means that no workflow is currently associated with the original stream.
    pub workflow_name: Option<String>,
}

pub fn start_reactor(
    name: String,
    executor: Box<dyn ReactorExecutor>,
    event_hub_subscriber: UnboundedSender<SubscriptionRequest>,
    update_interval: Duration,
) -> UnboundedSender<ReactorRequest> {
    let (sender, receiver) = unbounded_channel();
    let actor = Actor::new(
        name,
        receiver,
        executor,
        event_hub_subscriber,
        update_interval,
    );
    tokio::spawn(actor.run());

    sender
}

enum FutureResult {
    AllRequestConsumersGone,
    EventHubGone,
    WorkflowManagerGone,
    RequestReceived(ReactorRequest, UnboundedReceiver<ReactorRequest>),
    ExecutorResponseReceived {
        stream_name: String,
        workflow: Option<WorkflowDefinition>,
    },

    WorkflowManagerEventReceived(
        WorkflowManagerEvent,
        UnboundedReceiver<WorkflowManagerEvent>,
    ),

    ClientResponseChannelClosed {
        stream_name: String,
    },

    UpdateStreamNameRequested {
        stream_name: String,
    },
}

struct CachedWorkflow {
    definition: WorkflowDefinition,
}

struct Actor {
    name: String,
    executor: Box<dyn ReactorExecutor>,
    futures: FuturesUnordered<BoxFuture<'static, FutureResult>>,
    workflow_manager: Option<UnboundedSender<WorkflowManagerRequest>>,
    cached_workflows_for_stream_name: HashMap<String, CachedWorkflow>,
    update_interval: Duration,
    stream_response_channels: HashMap<String, Vec<UnboundedSender<ReactorWorkflowUpdate>>>,
}

unsafe impl Send for Actor {}

impl Actor {
    fn new(
        name: String,
        receiver: UnboundedReceiver<ReactorRequest>,
        executor: Box<dyn ReactorExecutor>,
        event_hub_subscriber: UnboundedSender<SubscriptionRequest>,
        update_interval: Duration,
    ) -> Self {
        let futures = FuturesUnordered::new();
        futures.push(wait_for_request(receiver).boxed());

        let (manager_sender, manager_receiver) = unbounded_channel();
        let _ = event_hub_subscriber.send(SubscriptionRequest::WorkflowManagerEvents {
            channel: manager_sender,
        });

        futures.push(wait_for_workflow_manager_event(manager_receiver).boxed());

        Actor {
            name,
            executor,
            futures,
            workflow_manager: None,
            cached_workflows_for_stream_name: HashMap::new(),
            update_interval,
            stream_response_channels: HashMap::new(),
        }
    }

    #[instrument(name = "Reactor Execution", skip(self), fields(name = %self.name))]
    async fn run(mut self) {
        info!("Starting reactor");

        while let Some(result) = self.futures.next().await {
            match result {
                FutureResult::AllRequestConsumersGone => {
                    info!("All consumers gone");
                    break;
                }

                FutureResult::EventHubGone => {
                    info!("Event manager gone");
                    break;
                }

                FutureResult::WorkflowManagerGone => {
                    info!("Workflow manager gone");
                    break;
                }

                FutureResult::ClientResponseChannelClosed { stream_name } => {
                    self.handle_response_channel_closed(stream_name);
                }

                FutureResult::RequestReceived(request, receiver) => {
                    self.futures.push(wait_for_request(receiver).boxed());
                    self.handle_request(request);
                }

                FutureResult::ExecutorResponseReceived {
                    stream_name,
                    workflow,
                } => {
                    self.handle_executor_response(stream_name, workflow);
                }

                FutureResult::UpdateStreamNameRequested { stream_name } => {
                    if self
                        .cached_workflows_for_stream_name
                        .contains_key(&stream_name)
                    {
                        let future = self.executor.get_workflow(stream_name.clone());
                        self.futures
                            .push(wait_for_executor_response(stream_name, future).boxed());
                    }
                }

                FutureResult::WorkflowManagerEventReceived(event, receiver) => {
                    self.futures
                        .push(wait_for_workflow_manager_event(receiver).boxed());

                    self.handle_workflow_manager_event(event);
                }
            }
        }

        info!("Reactor closing");
    }

    fn handle_request(&mut self, request: ReactorRequest) {
        match request {
            ReactorRequest::CreateWorkflowNameForStream {
                stream_name,
                response_channel,
            } => {
                info!(
                    stream_name = %stream_name,
                    "Received request to get workflow for stream '{}'", stream_name
                );

                let channels = self
                    .stream_response_channels
                    .entry(stream_name.clone())
                    .or_insert(Vec::new());

                channels.push(response_channel.clone());

                if let Some(cache) = self.cached_workflows_for_stream_name.get_mut(&stream_name) {
                    let _ = response_channel.send(ReactorWorkflowUpdate {
                        workflow_name: Some(cache.definition.name.clone()),
                    });
                } else {
                    let future = self.executor.get_workflow(stream_name.clone());
                    self.futures
                        .push(wait_for_executor_response(stream_name.clone(), future).boxed());
                }

                self.futures.push(
                    notify_when_response_channel_closed(response_channel, stream_name).boxed(),
                );
            }
        }
    }

    fn handle_executor_response(
        &mut self,
        stream_name: String,
        workflow: Option<WorkflowDefinition>,
    ) {
        if let Some(workflow) = workflow {
            info!(
                stream_name = %stream_name,
                workflow_name = %workflow.name,
                "Executor returned a workflow with the name {} for the stream {}",
                workflow.name, stream_name,
            );

            let workflow_name = workflow.name.clone();
            if let Some(cache) = self.cached_workflows_for_stream_name.get_mut(&stream_name) {
                if cache.definition.name != workflow_name {
                    // Since the executor returned a different workflow as a response, we need to
                    // shut the old one down.
                    if let Some(manager) = &self.workflow_manager {
                        let _ = manager.send(WorkflowManagerRequest {
                            request_id: format!("reactor_{}_stream_{}", self.name, stream_name),
                            operation: WorkflowManagerRequestOperation::StopWorkflow {
                                name: cache.definition.name.clone(),
                            },
                        });
                    }
                }

                cache.definition = workflow.clone();
            } else {
                self.cached_workflows_for_stream_name.insert(
                    stream_name.clone(),
                    CachedWorkflow {
                        definition: workflow.clone(),
                    },
                );
            }

            if let Some(manager) = &self.workflow_manager {
                let _ = manager.send(WorkflowManagerRequest {
                    request_id: format!("reactor_{}_stream_{}", self.name, stream_name),
                    operation: WorkflowManagerRequestOperation::UpsertWorkflow {
                        definition: workflow,
                    },
                });
            }

            if let Some(channels) = self.stream_response_channels.get(&stream_name) {
                for channel in channels {
                    let _ = channel.send(ReactorWorkflowUpdate {
                        workflow_name: Some(workflow_name.clone()),
                    });
                }
            }

            if !self.update_interval.is_zero() {
                self.futures.push(
                    wait_for_update_interval(stream_name, self.update_interval.clone()).boxed(),
                );
            }
        } else {
            info!(
                stream_name = %stream_name,
                "Executor returned no workflow for the stream {}", stream_name,
            );

            if let Some(channels) = self.stream_response_channels.remove(&stream_name) {
                for channel in channels {
                    let _ = channel.send(ReactorWorkflowUpdate {
                        workflow_name: None,
                    });
                }
            }

            if let Some(cache) = self.cached_workflows_for_stream_name.remove(&stream_name) {
                // Since we had a workflow cached, and now the external service isn't giving us one
                // that means the workflow we created is no longer valid and should be shut down.
                if let Some(workflow_manager) = &self.workflow_manager {
                    let _ = workflow_manager.send(WorkflowManagerRequest {
                        request_id: format!("reactor_{}_stream_{}", self.name, stream_name),
                        operation: WorkflowManagerRequestOperation::StopWorkflow {
                            name: cache.definition.name.clone(),
                        },
                    });
                }
            }
        }
    }

    fn handle_workflow_manager_event(&mut self, event: WorkflowManagerEvent) {
        match event {
            WorkflowManagerEvent::WorkflowManagerRegistered { channel } => {
                info!("Reactor received a workflow manager channel");
                self.futures
                    .push(notify_workflow_manager_gone(channel.clone()).boxed());

                // Upsert all cached workflows
                for workflow in self.cached_workflows_for_stream_name.values() {
                    let _ = channel.send(WorkflowManagerRequest {
                        request_id: format!("reactor_{}_cache_catchup", self.name),
                        operation: WorkflowManagerRequestOperation::UpsertWorkflow {
                            definition: workflow.definition.clone(),
                        },
                    });
                }

                self.workflow_manager = Some(channel);
            }
        }
    }

    fn handle_response_channel_closed(&mut self, stream_name: String) {
        if let Some(channels) = self.stream_response_channels.get_mut(&stream_name) {
            for x in (0..channels.len()).rev() {
                if channels[x].is_closed() {
                    channels.remove(x);
                }
            }

            if channels.is_empty() {
                info!(
                    stream_name = %stream_name,
                    "All response channels for stream {} closed", stream_name
                );

                self.stream_response_channels.remove(&stream_name);

                if let Some(cache) = self.cached_workflows_for_stream_name.remove(&stream_name) {
                    if let Some(channel) = &self.workflow_manager {
                        let _ = channel.send(WorkflowManagerRequest {
                            request_id: "from_reactor".to_string(),
                            operation: WorkflowManagerRequestOperation::StopWorkflow {
                                name: cache.definition.name.to_string(),
                            },
                        });
                    }
                }
            } else {
                info!(
                    stream_name = %stream_name,
                    "Response channel for stream {} closed but {} still remain",
                    stream_name, channels.len(),
                );
            }
        }
    }
}

async fn wait_for_request(mut receiver: UnboundedReceiver<ReactorRequest>) -> FutureResult {
    match receiver.recv().await {
        Some(request) => FutureResult::RequestReceived(request, receiver),
        None => FutureResult::AllRequestConsumersGone,
    }
}

async fn wait_for_executor_response(
    stream_name: String,
    future: BoxFuture<'static, Option<WorkflowDefinition>>,
) -> FutureResult {
    let result = future.await;
    FutureResult::ExecutorResponseReceived {
        stream_name,
        workflow: result,
    }
}

async fn wait_for_workflow_manager_event(
    mut receiver: UnboundedReceiver<WorkflowManagerEvent>,
) -> FutureResult {
    match receiver.recv().await {
        Some(event) => FutureResult::WorkflowManagerEventReceived(event, receiver),
        None => FutureResult::EventHubGone,
    }
}

async fn notify_workflow_manager_gone(
    sender: UnboundedSender<WorkflowManagerRequest>,
) -> FutureResult {
    sender.closed().await;
    FutureResult::WorkflowManagerGone
}

async fn notify_when_response_channel_closed(
    channel: UnboundedSender<ReactorWorkflowUpdate>,
    stream_name: String,
) -> FutureResult {
    channel.closed().await;
    FutureResult::ClientResponseChannelClosed { stream_name }
}

async fn wait_for_update_interval(stream_name: String, wait_time: Duration) -> FutureResult {
    tokio::time::sleep(wait_time).await;
    FutureResult::UpdateStreamNameRequested { stream_name }
}
