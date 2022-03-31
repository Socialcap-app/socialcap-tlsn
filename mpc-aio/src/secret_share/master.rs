use super::SecretShareError;
use crate::twopc::TwoPCProtocol;
use async_trait::async_trait;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use mpc_core::secret_share::{SecretShare, SecretShareMasterCore, SecretShareMessage};
use p256::EncodedPoint;
use std::io::Error as IOError;
use std::io::ErrorKind;

pub struct SecretShareMaster;

impl SecretShareMaster {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TwoPCProtocol<SecretShareMessage> for SecretShareMaster {
    type Input = EncodedPoint;
    type Error = SecretShareError;
    type Output = SecretShare;

    async fn run<
        S: Sink<SecretShareMessage> + Stream<Item = Result<SecretShareMessage, E>> + Send + Unpin,
        E: std::fmt::Debug,
    >(
        &mut self,
        stream: &mut S,
        input: Self::Input,
    ) -> Result<SecretShare, SecretShareError>
    where
        Self::Error: From<<S as Sink<SecretShareMessage>>::Error>,
        Self::Error: From<E>,
    {
        let master = SecretShareMasterCore::new(&input);

        // Step 1
        let (message, master) = master.next();
        stream.send(message.into()).await?;
        let slave_message = match stream.next().await {
            Some(Ok(SecretShareMessage::S1(m))) => m,
            Some(Ok(m)) => return Err(SecretShareError::Unexpected(m)),
            Some(Err(e)) => return Err(e)?,
            None => return Err(IOError::new(ErrorKind::UnexpectedEof, ""))?,
        };

        // Step 2
        let (message, master) = master.next(slave_message);
        stream.send(message.into()).await?;
        let slave_message = match stream.next().await {
            Some(Ok(SecretShareMessage::S2(m))) => m,
            Some(Ok(m)) => return Err(SecretShareError::Unexpected(m)),
            Some(Err(e)) => return Err(e)?,
            None => return Err(IOError::new(ErrorKind::UnexpectedEof, ""))?,
        };

        // Step 3
        let (message, master) = master.next(slave_message);
        stream.send(message.into()).await?;
        let slave_message = match stream.next().await {
            Some(Ok(SecretShareMessage::S3(m))) => m,
            Some(Ok(m)) => return Err(SecretShareError::Unexpected(m)),
            Some(Err(e)) => return Err(e)?,
            None => return Err(IOError::new(ErrorKind::UnexpectedEof, ""))?,
        };

        // Complete
        let master = master.next(slave_message);

        Ok(master.secret())
    }
}
