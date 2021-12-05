//! The RTMP Receive step registers with the RTMP server endpoint to allow publishers to connect with
//! the specified port, application name, and stream key combination.  Any media packets that
//! RTMP publishers send in will be sent to the next steps.
//!
//! All media packets that come in from previous workflow steps are ignored.
#[cfg(test)]
mod tests;

use crate::endpoints::rtmp_server::{
    IpRestriction, RegistrationType, RtmpEndpointPublisherMessage, RtmpEndpointRequest,
    StreamKeyRegistration,
};
use crate::net::{ConnectionId, IpAddress, IpAddressParseError};
use crate::workflows::definitions::WorkflowStepDefinition;
use crate::workflows::steps::factory::StepGenerator;
use crate::workflows::steps::{
    StepCreationResult, StepFutureResult, StepInputs, StepOutputs, StepStatus, WorkflowStep,
};
use crate::workflows::{MediaNotification, MediaNotificationContent};
use crate::StreamId;
use futures::FutureExt;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error as ThisError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, info};

pub const PORT_PROPERTY_NAME: &'static str = "port";
pub const APP_PROPERTY_NAME: &'static str = "rtmp_app";
pub const STREAM_KEY_PROPERTY_NAME: &'static str = "stream_key";
pub const IP_ALLOW_PROPERTY_NAME: &'static str = "allow_ips";
pub const IP_DENY_PROPERTY_NAME: &'static str = "deny_ips";
pub const RTMPS_FLAG: &'static str = "rtmps";

/// Generates new rtmp receiver workflow step instances based on specified step definitions.
pub struct RtmpReceiverStepGenerator {
    rtmp_endpoint_sender: UnboundedSender<RtmpEndpointRequest>,
}

struct RtmpReceiverStep {
    definition: WorkflowStepDefinition,
    rtmp_endpoint_sender: UnboundedSender<RtmpEndpointRequest>,
    port: u16,
    rtmp_app: String,
    stream_key: StreamKeyRegistration,
    status: StepStatus,
    connection_stream_id_map: HashMap<ConnectionId, StreamId>,
}

impl StepFutureResult for RtmpReceiveFutureResult {}

enum RtmpReceiveFutureResult {
    RtmpEndpointResponseReceived(
        RtmpEndpointPublisherMessage,
        UnboundedReceiver<RtmpEndpointPublisherMessage>,
    ),
    RtmpEndpointGone,
}

#[derive(ThisError, Debug)]
enum StepStartupError {
    #[error(
        "No RTMP app specified.  A non-empty parameter of '{}' is required",
        PORT_PROPERTY_NAME
    )]
    NoRtmpAppSpecified,

    #[error(
        "No stream key specified.  A non-empty parameter of '{}' is required",
        APP_PROPERTY_NAME
    )]
    NoStreamKeySpecified,

    #[error(
        "Invalid port value of '{0}' specified.  A number from 0 to 65535 should be specified"
    )]
    InvalidPortSpecified(String),

    #[error("Failed to parse ip address")]
    InvalidIpAddressSpecified(#[from] IpAddressParseError),

    #[error(
        "Both {} and {} were specified, but only one is allowed",
        IP_ALLOW_PROPERTY_NAME,
        IP_DENY_PROPERTY_NAME
    )]
    BothDenyAndAllowIpRestrictionsSpecified,
}

impl RtmpReceiverStepGenerator {
    pub fn new(rtmp_endpoint_sender: UnboundedSender<RtmpEndpointRequest>) -> Self {
        RtmpReceiverStepGenerator {
            rtmp_endpoint_sender,
        }
    }
}

impl StepGenerator for RtmpReceiverStepGenerator {
    fn generate(&self, definition: WorkflowStepDefinition) -> StepCreationResult {
        let use_rtmps = match definition.parameters.get(RTMPS_FLAG) {
            Some(_) => true,
            None => false,
        };

        let port = match definition.parameters.get(PORT_PROPERTY_NAME) {
            Some(Some(value)) => match value.parse::<u16>() {
                Ok(num) => num,
                Err(_) => {
                    return Err(Box::new(StepStartupError::InvalidPortSpecified(
                        value.clone(),
                    )));
                }
            },

            _ => {
                if use_rtmps {
                    443
                } else {
                    1935
                }
            }
        };

        let app = match definition.parameters.get(APP_PROPERTY_NAME) {
            Some(Some(x)) => x.trim(),
            _ => return Err(Box::new(StepStartupError::NoRtmpAppSpecified)),
        };

        let stream_key = match definition.parameters.get(STREAM_KEY_PROPERTY_NAME) {
            Some(Some(x)) => x.trim(),
            _ => return Err(Box::new(StepStartupError::NoStreamKeySpecified)),
        };

        let allowed_ips = match definition.parameters.get(IP_ALLOW_PROPERTY_NAME) {
            Some(Some(value)) => IpAddress::parse_comma_delimited_list(Some(value))?,
            _ => Vec::new(),
        };

        let denied_ips = match definition.parameters.get(IP_DENY_PROPERTY_NAME) {
            Some(Some(value)) => IpAddress::parse_comma_delimited_list(Some(value))?,
            _ => Vec::new(),
        };

        let ip_restriction = match (allowed_ips.len() > 0, denied_ips.len() > 0) {
            (true, true) => {
                return Err(Box::new(
                    StepStartupError::BothDenyAndAllowIpRestrictionsSpecified,
                ));
            }
            (true, false) => IpRestriction::Allow(allowed_ips),
            (false, true) => IpRestriction::Deny(denied_ips),
            (false, false) => IpRestriction::None,
        };

        let step = RtmpReceiverStep {
            definition: definition.clone(),
            status: StepStatus::Created,
            rtmp_endpoint_sender: self.rtmp_endpoint_sender.clone(),
            port,
            rtmp_app: app.to_string(),
            connection_stream_id_map: HashMap::new(),
            stream_key: if stream_key == "*" {
                StreamKeyRegistration::Any
            } else {
                StreamKeyRegistration::Exact(stream_key.to_string())
            },
        };

        let (sender, receiver) = unbounded_channel();
        let _ = step
            .rtmp_endpoint_sender
            .send(RtmpEndpointRequest::ListenForPublishers {
                message_channel: sender,
                port: step.port,
                rtmp_app: step.rtmp_app.clone(),
                rtmp_stream_key: step.stream_key.clone(),
                stream_id: None,
                ip_restrictions: ip_restriction,
                use_tls: use_rtmps,
            });

        Ok((
            Box::new(step),
            vec![wait_for_rtmp_endpoint_response(receiver).boxed()],
        ))
    }
}

impl RtmpReceiverStep {
    fn handle_rtmp_publisher_message(
        &mut self,
        outputs: &mut StepOutputs,
        message: RtmpEndpointPublisherMessage,
    ) {
        match message {
            RtmpEndpointPublisherMessage::PublisherRegistrationFailed => {
                error!("Rtmp receive step failed to register for publish registration");
                self.status = StepStatus::Error;

                return;
            }

            RtmpEndpointPublisherMessage::PublisherRegistrationSuccessful => {
                info!("Rtmp receive step successfully registered for publishing");
                self.status = StepStatus::Active;

                return;
            }

            RtmpEndpointPublisherMessage::NewPublisherConnected {
                stream_id,
                connection_id,
                stream_key,
            } => {
                info!(
                    stream_id = ?stream_id,
                    connection_id = ?connection_id,
                    stream_key = %stream_key,
                    "Rtmp receive step seen new publisher: {:?}, {:?}, {:?}", stream_id, connection_id, stream_key
                );

                self.connection_stream_id_map
                    .insert(connection_id, stream_id.clone());

                outputs.media.push(MediaNotification {
                    stream_id,
                    content: MediaNotificationContent::NewIncomingStream {
                        stream_name: stream_key,
                    },
                });
            }

            RtmpEndpointPublisherMessage::PublishingStopped { connection_id } => {
                match self.connection_stream_id_map.remove(&connection_id) {
                    None => (),
                    Some(stream_id) => {
                        info!(
                            stream_id = ?stream_id,
                            connection_id = ?connection_id,
                            "Rtmp receive step notified that connection {:?} is no longer publishing stream {:?}", connection_id, stream_id
                        );

                        outputs.media.push(MediaNotification {
                            stream_id,
                            content: MediaNotificationContent::StreamDisconnected,
                        });
                    }
                }
            }

            RtmpEndpointPublisherMessage::StreamMetadataChanged {
                publisher,
                metadata,
            } => match self.connection_stream_id_map.get(&publisher) {
                None => (),
                Some(stream_id) => outputs.media.push(MediaNotification {
                    stream_id: stream_id.clone(),
                    content: MediaNotificationContent::Metadata {
                        data: crate::utils::stream_metadata_to_hash_map(metadata),
                    },
                }),
            },

            RtmpEndpointPublisherMessage::NewVideoData {
                publisher,
                data,
                timestamp,
                codec,
                is_sequence_header,
                is_keyframe,
            } => match self.connection_stream_id_map.get(&publisher) {
                None => (),
                Some(stream_id) => {
                    outputs.media.push(MediaNotification {
                        stream_id: stream_id.clone(),
                        content: MediaNotificationContent::Video {
                            is_keyframe,
                            is_sequence_header,
                            data,
                            codec,
                            timestamp: Duration::from_millis(timestamp.value as u64),
                        },
                    });
                }
            },

            RtmpEndpointPublisherMessage::NewAudioData {
                publisher,
                is_sequence_header,
                data,
                codec,
                timestamp,
            } => match self.connection_stream_id_map.get(&publisher) {
                None => (),
                Some(stream_id) => {
                    outputs.media.push(MediaNotification {
                        stream_id: stream_id.clone(),
                        content: MediaNotificationContent::Audio {
                            is_sequence_header,
                            data,
                            codec,
                            timestamp: Duration::from_millis(timestamp.value as u64),
                        },
                    });
                }
            },
        }
    }
}

unsafe impl Send for RtmpReceiverStep {}

unsafe impl Sync for RtmpReceiverStep {}

impl WorkflowStep for RtmpReceiverStep {
    fn get_status(&self) -> &StepStatus {
        &self.status
    }

    fn get_definition(&self) -> &WorkflowStepDefinition {
        &self.definition
    }

    fn execute(&mut self, inputs: &mut StepInputs, outputs: &mut StepOutputs) {
        for future_result in inputs.notifications.drain(..) {
            let future_result = match future_result.downcast::<RtmpReceiveFutureResult>() {
                Ok(result) => *result,
                Err(_) => {
                    error!("Rtmp receive step received a notification that is not an 'RtmpReceiveFutureResult' type");
                    self.status = StepStatus::Error;

                    return;
                }
            };

            match future_result {
                RtmpReceiveFutureResult::RtmpEndpointGone => {
                    error!("Rtmp receive step stopping as the rtmp endpoint is gone");
                    self.status = StepStatus::Error;

                    return;
                }

                RtmpReceiveFutureResult::RtmpEndpointResponseReceived(message, receiver) => {
                    outputs
                        .futures
                        .push(wait_for_rtmp_endpoint_response(receiver).boxed());

                    self.handle_rtmp_publisher_message(outputs, message);
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.status = StepStatus::Shutdown;
        let _ = self
            .rtmp_endpoint_sender
            .send(RtmpEndpointRequest::RemoveRegistration {
                registration_type: RegistrationType::Publisher,
                port: self.port,
                rtmp_app: self.rtmp_app.clone(),
                rtmp_stream_key: self.stream_key.clone(),
            });
    }
}

async fn wait_for_rtmp_endpoint_response(
    mut receiver: UnboundedReceiver<RtmpEndpointPublisherMessage>,
) -> Box<dyn StepFutureResult> {
    let notification = match receiver.recv().await {
        None => RtmpReceiveFutureResult::RtmpEndpointGone,
        Some(message) => RtmpReceiveFutureResult::RtmpEndpointResponseReceived(message, receiver),
    };

    Box::new(notification)
}
