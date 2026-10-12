//! Titan WebSocket client. One connection serves every caller: a reader task routes each server
//! message by request id or stream id, so concurrent requests never consume each other's replies.
//! A connection that drops, or lets a request miss its deadline, is replaced on the next request.

use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    MaybeTlsStream, WebSocketStream,
};

use super::types::*;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum StreamEvent {
    Data(StreamData),
    End(StreamEnd),
}

/// Why a request failed: the connection broke (safe to retry elsewhere), or the server refused it.
enum RequestError {
    ConnectionLost(String),
    Server(String),
}

impl From<RequestError> for String {
    fn from(error: RequestError) -> Self {
        match error {
            RequestError::ConnectionLost(reason) => format!("Titan connection lost: {}", reason),
            RequestError::Server(message) => message,
        }
    }
}

struct Pending {
    reply: oneshot::Sender<Result<ResponseSuccess, RequestError>>,
    /// Where the stream this request starts goes. The reader registers it on the response, so no
    /// stream data can arrive before its consumer exists.
    stream_tx: Option<mpsc::UnboundedSender<StreamEvent>>,
}

#[derive(Default)]
struct Routes {
    pending: HashMap<u32, Pending>,
    streams: HashMap<u32, mpsc::UnboundedSender<StreamEvent>>,
}

struct Connection {
    sink: Mutex<SplitSink<WsStream, Message>>,
    routes: Arc<StdMutex<Routes>>,
    alive: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Connection {
    async fn open(endpoint: &str, jwt_token: &str) -> Result<Self, String> {
        let request = build_ws_request(endpoint, jwt_token)?;
        let (ws, _response) = connect_async(request).await.map_err(|e| {
            let error_msg = format!("Failed to connect: {}", e);
            if error_msg.contains("403") {
                format!("{} - Authentication failed. Please verify:\n  - API key is valid and not expired\n  - API key has access to partners.api.titan.exchange\n  - API key audience is 'api.titan.ag'", error_msg)
            } else {
                error_msg
            }
        })?;

        let (sink, stream) = ws.split();
        let routes = Arc::new(StdMutex::new(Routes::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader = tokio::spawn(read_loop(stream, Arc::clone(&routes), Arc::clone(&alive)));

        Ok(Self {
            sink: Mutex::new(sink),
            routes,
            alive,
            reader,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Replace this connection on the next request; it closes once in-flight requests drop it.
    fn retire(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    async fn send(&self, request: &ClientRequest) -> Result<(), String> {
        // Titan requires structs to be encoded as maps, not arrays
        let encoded = rmp_serde::encode::to_vec_named(request)
            .map_err(|e| format!("Failed to encode request: {}", e))?;
        self.sink
            .lock()
            .await
            .send(Message::Binary(bytes::Bytes::from(encoded)))
            .await
            .map_err(|e| format!("Failed to send: {}", e))
    }

    fn forget(&self, request_id: u32) {
        self.routes.lock().unwrap().pending.remove(&request_id);
    }

    fn forget_stream(&self, stream_id: u32) {
        self.routes.lock().unwrap().streams.remove(&stream_id);
    }
}

async fn read_loop(
    mut stream: SplitStream<WsStream>,
    routes: Arc<StdMutex<Routes>>,
    alive: Arc<AtomicBool>,
) {
    let reason = loop {
        match stream.next().await {
            None => break "connection closed".to_string(),
            Some(Err(e)) => break format!("read failed: {}", e),
            Some(Ok(Message::Binary(data))) => {
                match rmp_serde::from_slice::<ServerMessage>(&data) {
                    Ok(message) => dispatch(&routes, message),
                    Err(e) => tracing::warn!(error = %e, "Titan: undecodable server message"),
                }
            }
            Some(Ok(Message::Close(frame))) => {
                break format!("server closed the connection: {:?}", frame)
            }
            // Pings are answered by tungstenite itself; nothing else carries protocol data.
            Some(Ok(_)) => {}
        }
    };

    alive.store(false, Ordering::SeqCst);
    tracing::warn!(reason = %reason, "Titan connection lost");
    let mut routes = routes.lock().unwrap();
    for (_, pending) in routes.pending.drain() {
        let _ = pending
            .reply
            .send(Err(RequestError::ConnectionLost(reason.clone())));
    }
    // Dropping the senders ends every open stream.
    routes.streams.clear();
}

fn dispatch(routes: &StdMutex<Routes>, message: ServerMessage) {
    let mut routes = routes.lock().unwrap();
    match message {
        ServerMessage::Response(response) => {
            let Some(pending) = routes.pending.remove(&response.request_id) else {
                tracing::debug!(
                    request_id = response.request_id,
                    "Titan: response for an abandoned request"
                );
                return;
            };
            if let (Some(stream), Some(stream_tx)) = (response.stream.as_ref(), pending.stream_tx) {
                routes.streams.insert(stream.id, stream_tx);
            }
            let _ = pending.reply.send(Ok(response));
        }
        ServerMessage::Error(error) => {
            if let Some(pending) = routes.pending.remove(&error.request_id) {
                let _ = pending.reply.send(Err(RequestError::Server(format!(
                    "Server error: {} (code {})",
                    error.message, error.code
                ))));
            }
        }
        ServerMessage::StreamData(data) => {
            let id = data.id;
            if let Some(stream_tx) = routes.streams.get(&id) {
                if stream_tx.send(StreamEvent::Data(data)).is_err() {
                    routes.streams.remove(&id);
                }
            }
        }
        ServerMessage::StreamEnd(end) => {
            if let Some(stream_tx) = routes.streams.remove(&end.id) {
                let _ = stream_tx.send(StreamEvent::End(end));
            }
        }
    }
}

/// `endpoint` is a Titan host (`us1.api.titan.exchange`); a full `ws(s)://` URL is used as is.
fn build_ws_request(
    endpoint: &str,
    jwt_token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, String> {
    let url = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("wss://{endpoint}/api/v1/ws")
    };
    // Titan takes the token as a query parameter and as a Bearer header; send both.
    let mut request = format!("{url}?auth={}", urlencoding::encode(jwt_token))
        .into_client_request()
        .map_err(|e| format!("Invalid Titan endpoint: {e}"))?;
    let headers = request.headers_mut();
    headers.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("v1.api.titan.ag"),
    );
    headers.insert(
        "Authorization",
        format!("Bearer {jwt_token}")
            .parse()
            .map_err(|e| format!("Invalid Titan API key: {e}"))?,
    );
    Ok(request)
}

pub(crate) struct TitanClient {
    endpoint: String,
    jwt_token: String,
    request_timeout: Duration,
    connection: Mutex<Option<Arc<Connection>>>,
    next_request_id: AtomicU32,
}

impl TitanClient {
    /// Connects lazily, on the first request. `request_timeout` bounds connecting and each request
    /// (including the wait for the first quote).
    pub fn new(endpoint: String, jwt_token: String, request_timeout: Duration) -> Self {
        TitanClient {
            endpoint,
            jwt_token,
            request_timeout,
            connection: Mutex::new(None),
            next_request_id: AtomicU32::new(1),
        }
    }

    /// The live connection, reconnecting if there is none.
    async fn connection(&self) -> Result<Arc<Connection>, String> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref() {
            if connection.is_alive() {
                return Ok(Arc::clone(connection));
            }
        }
        *slot = None;

        let connection = timeout(
            self.request_timeout,
            Connection::open(&self.endpoint, &self.jwt_token),
        )
        .await
        .map_err(|_| "Timed out connecting to Titan".to_string())??;
        let connection = Arc::new(connection);
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    /// Send `data` and wait for its response, by `deadline`. Returns the connection that served
    /// it. If the connection breaks before a response arrives, the request is retried once on a
    /// fresh connection (every request we send is idempotent).
    async fn request(
        &self,
        data: RequestData,
        stream_tx: Option<mpsc::UnboundedSender<StreamEvent>>,
        deadline: Instant,
    ) -> Result<(Arc<Connection>, ResponseSuccess), String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let connection = tokio::time::timeout_at(deadline, self.connection())
                .await
                .map_err(|_| "Timed out connecting to Titan".to_string())??;

            let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
            let (reply_tx, reply_rx) = oneshot::channel();
            connection.routes.lock().unwrap().pending.insert(
                id,
                Pending {
                    reply: reply_tx,
                    stream_tx: stream_tx.clone(),
                },
            );

            let outcome = match connection
                .send(&ClientRequest {
                    id,
                    data: data.clone(),
                })
                .await
            {
                Err(e) => Err(RequestError::ConnectionLost(e)),
                Ok(()) => match tokio::time::timeout_at(deadline, reply_rx).await {
                    Err(_) => {
                        connection.forget(id);
                        connection.retire();
                        return Err("Titan request timed out".to_string());
                    }
                    Ok(Err(_)) => Err(RequestError::ConnectionLost("reader stopped".to_string())),
                    Ok(Ok(reply)) => reply,
                },
            };

            match outcome {
                Ok(response) => return Ok((connection, response)),
                Err(RequestError::ConnectionLost(reason)) if attempt < 2 => {
                    connection.forget(id);
                    connection.retire();
                    tracing::debug!(reason = %reason, "Titan: retrying request on a new connection");
                }
                Err(error) => {
                    connection.forget(id);
                    if matches!(error, RequestError::ConnectionLost(_)) {
                        connection.retire();
                    }
                    return Err(error.into());
                }
            }
        }
    }

    /// The best route (and its provider) from the first quote-stream update that has one. Routes
    /// without instructions (prebuilt transaction only) are skipped: we add the compute budget.
    pub async fn request_swap_quotes(
        &self,
        request: SwapQuoteRequest,
    ) -> Result<(String, SwapRoute), String> {
        let deadline = Instant::now() + self.request_timeout;

        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
        let (connection, response) = self
            .request(
                RequestData::NewSwapQuoteStream(request),
                Some(stream_tx),
                deadline,
            )
            .await?;
        let stream_id = response
            .stream
            .ok_or_else(|| "No stream started".to_string())?
            .id;

        let best = tokio::time::timeout_at(deadline, async {
            loop {
                match stream_rx.recv().await {
                    Some(StreamEvent::Data(data)) => {
                        let StreamDataPayload::SwapQuotes(quotes) = data.payload;
                        let best = quotes
                            .quotes
                            .into_iter()
                            .filter(|(_, route)| !route.instructions.is_empty())
                            .max_by_key(|(_, route)| route.out_amount);
                        if let Some(best) = best {
                            return Ok(best);
                        }
                        // No executable route in this update yet; wait for the next one.
                    }
                    Some(StreamEvent::End(end)) => {
                        return Err(match end.error_message {
                            Some(message) => format!("Stream ended with error: {}", message),
                            None => "Stream ended without quotes".to_string(),
                        });
                    }
                    None => return Err("Titan connection lost".to_string()),
                }
            }
        })
        .await;

        // We only ever need one update; the stop confirmation is ignored.
        connection.forget_stream(stream_id);
        let stop = ClientRequest {
            id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            data: RequestData::StopStream(StopStreamRequest { id: stream_id }),
        };
        if let Err(e) = connection.send(&stop).await {
            tracing::debug!(error = %e, stream_id, "Titan: failed to stop quote stream");
        }

        best.map_err(|_| {
            connection.retire();
            "Timed out waiting for a Titan quote".to_string()
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    fn route(out_amount: u64) -> SwapRoute {
        SwapRoute {
            out_amount,
            instructions: vec![Instruction {
                p: [7; 32],
                a: vec![],
                d: vec![1],
            }],
            address_lookup_tables: vec![],
            expires_at_ms: None,
            compute_units: None,
            compute_units_safe: Some(200_000),
        }
    }

    fn encode(message: &ServerMessage) -> Message {
        Message::Binary(bytes::Bytes::from(
            rmp_serde::encode::to_vec_named(message).unwrap(),
        ))
    }

    fn quote_request(amount: u64) -> SwapQuoteRequest {
        SwapQuoteRequest {
            swap: SwapParams {
                input_mint: [1; 32],
                output_mint: [2; 32],
                amount,
                swap_mode: Some(SwapMode::ExactIn),
                slippage_bps: Some(50),
            },
            transaction: TransactionParams {
                user_public_key: [3; 32],
                close_input_token_account: None,
                create_output_token_account: None,
                output_wsol: None,
            },
            update: None,
        }
    }

    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Answer every quote request with a stream whose first update quotes `amount * 2`.
        /// Requests on the same connection are answered in reverse order, interleaved with pings.
        Answer,
        /// Accept the connection but never answer anything.
        Silent,
        /// Answer the first request on each connection, then drop the connection.
        AnswerOnceThenDrop,
    }

    /// A local Titan stand-in. Returns its `ws://` endpoint and a counter of connections.
    async fn mock_titan(behaviour: Behaviour) -> (String, Arc<AtomicU32>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/ws", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&connections);

        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // Agree to the subprotocol like Titan does, or the client rejects the handshake.
                    let mut ws = accept_hdr_async(tcp, |_req: &Request, mut resp: Response| {
                        resp.headers_mut().insert(
                            "Sec-WebSocket-Protocol",
                            HeaderValue::from_static("v1.api.titan.ag"),
                        );
                        Ok(resp)
                    })
                    .await
                    .unwrap();
                    let mut held: Vec<ClientRequest> = Vec::new();
                    let mut next_stream = 100;
                    while let Some(Ok(message)) = ws.next().await {
                        let Message::Binary(data) = message else {
                            continue;
                        };
                        let request: ClientRequest = rmp_serde::from_slice(&data).unwrap();
                        let RequestData::NewSwapQuoteStream(_) = &request.data else {
                            continue; // StopStream etc.
                        };
                        match behaviour {
                            Behaviour::Silent => continue,
                            Behaviour::Answer => {
                                // Hold requests in pairs and answer the later one first, so a
                                // client that reads replies in send order would mix them up.
                                held.push(request);
                                if held.len() < 2 {
                                    continue;
                                }
                            }
                            Behaviour::AnswerOnceThenDrop => held.push(request),
                        }
                        for request in held.drain(..).rev() {
                            let RequestData::NewSwapQuoteStream(quote) = &request.data else {
                                unreachable!()
                            };
                            let stream_id = next_stream;
                            next_stream += 1;
                            ws.send(Message::Ping(bytes::Bytes::from_static(b"hi")))
                                .await
                                .unwrap();
                            ws.send(encode(&ServerMessage::Response(ResponseSuccess {
                                request_id: request.id,
                                stream: Some(StreamStart { id: stream_id }),
                            })))
                            .await
                            .unwrap();
                            ws.send(encode(&ServerMessage::StreamData(StreamData {
                                id: stream_id,
                                payload: StreamDataPayload::SwapQuotes(SwapQuotes {
                                    quotes: HashMap::from([(
                                        "mock".to_string(),
                                        route(quote.swap.amount * 2),
                                    )]),
                                }),
                            })))
                            .await
                            .unwrap();
                        }
                        if matches!(behaviour, Behaviour::AnswerOnceThenDrop) {
                            let _ = ws.close(None).await;
                            return;
                        }
                    }
                });
            }
        });

        (endpoint, connections)
    }

    #[tokio::test]
    async fn concurrent_quotes_each_get_their_own_reply() {
        let (endpoint, _) = mock_titan(Behaviour::Answer).await;
        let client = Arc::new(TitanClient::new(
            endpoint,
            String::new(),
            Duration::from_secs(5),
        ));

        for round in 0..20u64 {
            let (a, b) = tokio::join!(
                client.request_swap_quotes(quote_request(1_000 + round)),
                client.request_swap_quotes(quote_request(5_000 + round)),
            );
            assert_eq!(a.unwrap().1.out_amount, (1_000 + round) * 2);
            assert_eq!(b.unwrap().1.out_amount, (5_000 + round) * 2);
        }
    }

    #[tokio::test]
    async fn unanswered_request_times_out_instead_of_hanging() {
        let (endpoint, connections) = mock_titan(Behaviour::Silent).await;
        let client = TitanClient::new(endpoint, String::new(), Duration::from_millis(300));

        let started = std::time::Instant::now();
        let error = client
            .request_swap_quotes(quote_request(1))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"), "{}", error);
        assert!(started.elapsed() < Duration::from_secs(2));

        // The timed-out connection is retired: the next request opens a fresh one.
        let _ = client.request_swap_quotes(quote_request(1)).await;
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reconnects_after_the_server_drops_the_connection() {
        let (endpoint, connections) = mock_titan(Behaviour::AnswerOnceThenDrop).await;
        let client = TitanClient::new(endpoint, String::new(), Duration::from_secs(5));

        for amount in [10, 20, 30] {
            let (_, route) = client
                .request_swap_quotes(quote_request(amount))
                .await
                .unwrap();
            assert_eq!(route.out_amount, amount * 2);
        }
        assert_eq!(connections.load(Ordering::SeqCst), 3);
    }
}
