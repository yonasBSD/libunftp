//! The RFC 959 Store File Uniquely (`STOU`) command

use crate::server::chancomms::DataChanCmd;
use crate::{
    auth::UserDetail,
    server::controlchan::{
        Reply, ReplyCode,
        error::ControlChanError,
        handler::{CommandContext, CommandHandler},
    },
    storage::{Metadata, StorageBackend},
};
use async_trait::async_trait;
use std::path::Path;
use uuid::Uuid;

// TODO: Write functional test for STOU command.
#[derive(Debug)]
pub struct Stou;

#[async_trait]
impl<Storager, User> CommandHandler<Storager, User> for Stou
where
    User: UserDetail + 'static,
    Storager: StorageBackend<User> + 'static,
    Storager::Metadata: Metadata,
{
    #[tracing_attributes::instrument]
    async fn handle(&self, args: CommandContext<Storager, User>) -> Result<Reply, ControlChanError> {
        let mut session = args.session.lock().await;
        let uuid: String = Uuid::new_v4().to_string();
        let filename: &Path = std::path::Path::new(&uuid);
        let path: String = session.cwd.join(filename).to_string_lossy().to_string();
        let logger = args.logger;
        match session.data_cmd_tx.take() {
            Some(tx) => {
                tokio::spawn(async move {
                    if let Err(err) = tx.send(DataChanCmd::Stor { path }).await {
                        slog::warn!(logger, "STOU: could not send Stor command over data channel. {}", err);
                    }
                });
                Ok(Reply::new_with_string(ReplyCode::FileStatusOkay, filename.to_string_lossy().to_string()))
            }
            None => {
                slog::warn!(logger, "STOU: no data connection established for STOU file {:?}", path);
                Ok(Reply::new(ReplyCode::CantOpenDataConnection, "No data connection established"))
            }
        }
    }
}
