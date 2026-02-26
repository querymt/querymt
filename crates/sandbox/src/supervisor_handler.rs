use crate::{
    ApprovalBackend, ApprovalDecision, NeverGrantChecker, SupervisorMessage, SupervisorResponse,
    SupervisorSocket, send_extension_token,
};

/// Platform-specific supervisor request handler.
///
/// Implementations encapsulate per-OS capability expansion behavior while
/// sharing the same request-response contract for the orchestrator loop.
pub trait SupervisorRequestHandler: Send {
    /// Run a single request-response cycle on the supervisor socket.
    ///
    /// Returns `Ok(true)` if a request was processed, `Ok(false)` if the worker
    /// disconnected, or `Err` on protocol errors.
    fn handle_request(
        &mut self,
        socket: &mut SupervisorSocket,
        backend: &dyn ApprovalBackend,
        never_grant: Option<&NeverGrantChecker>,
    ) -> nono::Result<bool>;
}

/// Build the platform-specific supervisor request handler.
pub fn platform_supervisor_handler() -> Box<dyn SupervisorRequestHandler> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacosSupervisorRequestHandler)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(DefaultSupervisorRequestHandler)
    }
}

#[cfg(not(target_os = "macos"))]
struct DefaultSupervisorRequestHandler;

#[cfg(not(target_os = "macos"))]
impl SupervisorRequestHandler for DefaultSupervisorRequestHandler {
    fn handle_request(
        &mut self,
        socket: &mut SupervisorSocket,
        backend: &dyn ApprovalBackend,
        never_grant: Option<&NeverGrantChecker>,
    ) -> nono::Result<bool> {
        crate::handle_supervisor_request(socket, backend, never_grant)
    }
}

#[cfg(target_os = "macos")]
struct MacosSupervisorRequestHandler;

#[cfg(target_os = "macos")]
impl SupervisorRequestHandler for MacosSupervisorRequestHandler {
    fn handle_request(
        &mut self,
        socket: &mut SupervisorSocket,
        backend: &dyn ApprovalBackend,
        never_grant: Option<&NeverGrantChecker>,
    ) -> nono::Result<bool> {
        let msg = match socket.recv_message() {
            Ok(msg) => msg,
            Err(nono::NonoError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        match msg {
            SupervisorMessage::Request(request) => {
                let request_id = request.request_id.clone();

                if let Some(checker) = never_grant
                    && checker.check(&request.path).is_blocked()
                {
                    let response = SupervisorResponse::Decision {
                        request_id,
                        decision: ApprovalDecision::Denied {
                            reason: format!(
                                "Path {} is on the never-grant list",
                                request.path.display()
                            ),
                        },
                    };
                    socket.send_response(&response)?;
                    return Ok(true);
                }

                let decision = backend.request_capability(&request)?;

                socket.send_response(&SupervisorResponse::Decision {
                    request_id,
                    decision: decision.clone(),
                })?;

                if decision.is_granted() {
                    match nono::sandbox::extension_issue_file(&request.path, request.access) {
                        Ok(token) => {
                            send_extension_token(socket, &token)?;
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %request.path.display(),
                                error = %e,
                                "Failed to issue sandbox extension token; sending error sentinel"
                            );
                            send_extension_token(socket, "__ERROR__")?;
                        }
                    }
                }

                Ok(true)
            }
        }
    }
}
