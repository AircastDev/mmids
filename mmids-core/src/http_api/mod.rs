mod get_workflow_details;
mod list_workflows;
pub mod routing;

pub mod handlers {
    pub use super::get_workflow_details::GetWorkflowDetailsHandler;
    pub use super::list_workflows::ListWorkflowsHandler;
}

use crate::http_api::routing::RoutingTable;
use crate::workflows::manager::WorkflowManagerRequest;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot::{channel, Receiver, Sender};
use tracing::{error, info, instrument};
use uuid::Uuid;

pub struct HttpApiShutdownSignal {}

pub fn start_http_api(
    bind_address: SocketAddr,
    routes: RoutingTable,
    manager: UnboundedSender<WorkflowManagerRequest>,
) -> Sender<HttpApiShutdownSignal> {
    let routes = Arc::new(routes);
    let service = make_service_fn(move |socket: &AddrStream| {
        let remote_address = socket.remote_addr();
        let manager_clone = manager.clone();
        let routes_clone = routes.clone();
        async move {
            Ok::<_, hyper::Error>(service_fn(move |request: Request<Body>| {
                execute_request(
                    request,
                    remote_address,
                    manager_clone.clone(),
                    routes_clone.clone(),
                )
            }))
        }
    });

    let (sender, receiver) = channel();
    let server = Server::bind(&bind_address)
        .serve(service)
        .with_graceful_shutdown(graceful_shutdown(receiver));

    info!("Starting HTTP api on {}", bind_address);
    tokio::spawn(async { server.await });

    sender
}

async fn graceful_shutdown(shutdown_signal: Receiver<HttpApiShutdownSignal>) {
    let _ = shutdown_signal.await;
}

#[instrument(
    skip(request, client_address, manager, routes),
    fields(
        http_method = %request.method(),
        http_uri = %request.uri(),
        client_ip = %client_address.ip(),
        request_id = %Uuid::new_v4(),
    )
)]
async fn execute_request(
    mut request: Request<Body>,
    client_address: SocketAddr,
    manager: UnboundedSender<WorkflowManagerRequest>,
    routes: Arc<RoutingTable>,
) -> Result<Response<Body>, hyper::Error> {
    info!(
        "Incoming HTTP request for {} {} from {}",
        request.method(),
        request.uri(),
        client_address.ip()
    );

    let started_at = Instant::now();

    let parts = request
        .uri()
        .path()
        .split('/')
        .filter(|x| x.trim() != "")
        .collect::<Vec<_>>();

    match routes.get_route(request.method(), &parts) {
        Some(route) => {
            let parameters = route.get_parameters(&parts);
            match route
                .handler
                .execute(&mut request, parameters, manager)
                .await
            {
                Ok(response) => {
                    let elapsed = started_at.elapsed();
                    info!(
                        duration = %elapsed.as_millis(),
                        "Request returning status code {} in {} ms", response.status(), elapsed.as_millis()
                    );

                    Ok(response)
                }

                Err(error) => {
                    let elapsed = started_at.elapsed();
                    error!(
                        duration = %elapsed.as_millis(),
                        "Request thrown error: {:?}", error
                    );

                    Err(error)
                }
            }
        }

        None => {
            info!("No route found for this URL, returning 404");
            let mut response = Response::new(Body::from("Invalid URL"));
            *response.status_mut() = StatusCode::NOT_FOUND;

            Ok(response)
        }
    }
}
