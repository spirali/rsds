use crate::common::data::SerializationType;
use crate::error::DsError::GenericError;
use crate::scheduler::TaskId;
use crate::transfer::messages::{DataRequest, DataResponse, FetchRequestMsg};
use bytes::BytesMut;
use futures::SinkExt;
use tokio::net::TcpStream;
use tokio::stream::StreamExt;
use crate::error::DsError;

pub async fn fetch_data(
    mut stream: tokio_util::codec::Framed<TcpStream, tokio_util::codec::LengthDelimitedCodec>,
    task_id: TaskId,
) -> crate::Result<(
    tokio_util::codec::Framed<TcpStream, tokio_util::codec::LengthDelimitedCodec>,
    BytesMut,
    SerializationType,
)> {
    let message = DataRequest::FetchRequest(FetchRequestMsg { task_id });
    let data = rmp_serde::to_vec_named(&message)?;
    stream.send(data.into()).await?;

    let message: DataResponse = {
        let data = match stream.next().await {
            None => return Err(GenericError("Unexpected close of connection".into())),
            Some(data) => data?,
        };
        rmp_serde::from_slice(&data)?
    };
    let header = match message {
        DataResponse::NotAvailable => {
            log::error!("Fetching data={} failed: not available", task_id);
            return Err(DsError::GenericError("Data object not available".into()));
        }
        DataResponse::Data(x) => x,
        DataResponse::DataUploaded(_) => {
            // Worker send complete garbage, it should be considered as invalid and termianted
            log::error!("Fetching data={} failed: invalid response", task_id);
            return Err(DsError::GenericError("Request returned invalid response".into()));
        }
    };
    let data = match stream.next().await {
        None => return Err(GenericError("Unexpected close of connection".into())),
        Some(data) => data?,
    };
    Ok((stream, data, header.serializer))
}