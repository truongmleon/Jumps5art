//! Defines the structured confirmation boundary between the daemon and Fabric.
//!
//! For example, a reply that says saving resumed is useful only when it belongs
//! to the exact command and active process generation. The daemon still owns
//! the authenticated socket, watchdog deadlines, and process identity checks.

use jumpstart_domain::{BlobId, JobId, PositiveCounter};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identifies one command in an accepted Worlds operation.
///
/// The durable job identifies the operation, the generation fences restarted
/// attempts, and the sequence rejects delayed responses from another command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldsExchange {
    pub operation_id: JobId,
    pub generation: PositiveCounter,
    pub command_seq: PositiveCounter,
}

/// Commands the daemon sends to its managed Fabric adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method", deny_unknown_fields)]
pub enum WorldsControlCommand {
    #[serde(rename = "health")]
    Health,
    #[serde(rename = "players.warn")]
    WarnPlayers { countdown_seconds: u16 },
    #[serde(rename = "admission.close")]
    CloseAdmission,
    #[serde(rename = "save.freeze")]
    FreezeSaving,
    #[serde(rename = "capture.complete")]
    CompleteCapture { manifest_hash: BlobId },
    #[serde(rename = "save.resume")]
    ResumeSaving,
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// One request tied to the adapter protocol pinned by the active configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldsControlRequest {
    pub protocol_version: PositiveCounter,
    pub exchange: WorldsExchange,
    pub command: WorldsControlCommand,
}

/// Actual state reported by the adapter after executing the command.
///
/// `world_loaded` and `saving_enabled` are independent of whether new players
/// can join. The state is evidence from a particular exchange, not a process
/// lifecycle or an acknowledgment of Java process exit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldsObservedState {
    pub world_loaded: bool,
    pub saving_enabled: bool,
    pub admission_open: bool,
}

/// The adapter's claimed outcome, separately checked against its observed state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorldsControlOutcome {
    Health,
    PlayersWarned,
    AdmissionClosed,
    SavingFrozen,
    CaptureCompleted { manifest_hash: BlobId },
    SavingResumed,
    FinalSaveConfirmed,
    Rejected,
}

/// A bounded control reply from the adapter on its authenticated connection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldsControlReply {
    pub protocol_version: PositiveCounter,
    pub exchange: WorldsExchange,
    pub state: WorldsObservedState,
    pub outcome: WorldsControlOutcome,
}

/// Reasons a reply cannot establish the requested Worlds safety condition.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum WorldsControlError {
    #[error("Worlds control reply uses a different adapter protocol")]
    ProtocolMismatch,
    #[error("Worlds control reply refers to a different operation, generation, or sequence")]
    WrongExchange,
    #[error("the Worlds adapter rejected the command")]
    Rejected,
    #[error("Worlds control reply confirms a different action or capture")]
    WrongOutcome,
    #[error("Worlds control reply does not confirm the required saving or admission state")]
    UnsafeState,
}

impl WorldsControlReply {
    /// Match this reply to `request` and return its observed state on success.
    ///
    /// Callers must authenticate the adapter and enforce the command deadline
    /// first. This rejects stale or mismatched replies and checks the saving
    /// and admission conditions required by the reported outcome.
    pub fn checked_state(
        &self,
        request: &WorldsControlRequest,
    ) -> Result<WorldsObservedState, WorldsControlError> {
        use WorldsControlCommand as Command;
        use WorldsControlOutcome as Outcome;

        if self.protocol_version != request.protocol_version {
            return Err(WorldsControlError::ProtocolMismatch);
        }
        if self.exchange != request.exchange {
            return Err(WorldsControlError::WrongExchange);
        }
        let state = self.state;
        let safe = match (request.command, self.outcome) {
            (_, Outcome::Rejected) => return Err(WorldsControlError::Rejected),
            (Command::Health, Outcome::Health)
            | (Command::WarnPlayers { .. }, Outcome::PlayersWarned) => true,
            (Command::CloseAdmission, Outcome::AdmissionClosed)
            | (Command::Shutdown, Outcome::FinalSaveConfirmed) => !state.admission_open,
            (Command::FreezeSaving, Outcome::SavingFrozen) => {
                state.world_loaded && !state.saving_enabled && !state.admission_open
            }
            (
                Command::CompleteCapture {
                    manifest_hash: expected,
                },
                Outcome::CaptureCompleted { manifest_hash },
            ) if expected == manifest_hash => {
                state.world_loaded && !state.saving_enabled && !state.admission_open
            }
            (Command::ResumeSaving, Outcome::SavingResumed) => {
                state.world_loaded && state.saving_enabled && !state.admission_open
            }
            _ => return Err(WorldsControlError::WrongOutcome),
        };
        if !safe {
            return Err(WorldsControlError::UnsafeState);
        }
        Ok(state)
    }

    /// Check a startup health request before the daemon permits player admission.
    ///
    /// Requires a matching health exchange, a loaded world, saving enabled,
    /// and admission still closed. The daemon still needs current process
    /// ownership and storage evidence before completing a start.
    pub fn check_startup_health(
        &self,
        request: &WorldsControlRequest,
    ) -> Result<(), WorldsControlError> {
        if request.command != WorldsControlCommand::Health {
            return Err(WorldsControlError::WrongOutcome);
        }
        let state = self.checked_state(request)?;
        if !state.world_loaded || !state.saving_enabled || state.admission_open {
            return Err(WorldsControlError::UnsafeState);
        }
        Ok(())
    }
}
