use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use serde_json::from_str;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Response;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, client_async_tls};
use fast_socks5::client::{Socks5Stream, Config as Socks5Config};
use url::Url;

use crate::config::Config;
use crate::errors::*;

pub static STREAM_ENDPOINT: &str = "stream";
pub static WS_ENDPOINT: &str = "ws";
pub static OUTBOUND_ACCOUNT_INFO: &str = "outboundAccountInfo";
pub static OUTBOUND_ACCOUNT_POSITION: &str = "outboundAccountPosition";
pub static EXECUTION_REPORT: &str = "executionReport";
pub static KLINE: &str = "kline";
pub static AGGREGATED_TRADE: &str = "aggTrade";
pub static DEPTH_ORDERBOOK: &str = "depthUpdate";
pub static PARTIAL_ORDERBOOK: &str = "lastUpdateId";
pub static DAYTICKER: &str = "24hrTicker";

pub fn all_ticker_stream() -> &'static str { "!ticker@arr" }

pub fn ticker_stream(symbol: &str) -> String { format!("{symbol}@ticker") }

pub fn agg_trade_stream(symbol: &str) -> String { format!("{symbol}@aggTrade") }

pub fn trade_stream(symbol: &str) -> String { format!("{symbol}@trade") }

pub fn kline_stream(symbol: &str, interval: &str) -> String { format!("{symbol}@kline_{interval}") }

pub fn book_ticker_stream(symbol: &str) -> String { format!("{symbol}@bookTicker") }

pub fn all_book_ticker_stream() -> &'static str { "!bookTicker" }

pub fn all_mini_ticker_stream() -> &'static str { "!miniTicker@arr" }

pub fn mini_ticker_stream(symbol: &str) -> String { format!("{symbol}@miniTicker") }

/// # Arguments
///
/// * `symbol`: the market symbol
/// * `levels`: 5, 10 or 20
/// * `update_speed`: 1000 or 100
pub fn partial_book_depth_stream(symbol: &str, levels: u16, update_speed: u16) -> String {
    format!("{symbol}@depth{levels}@{update_speed}ms")
}

/// # Arguments
///
/// * `symbol`: the market symbol
/// * `update_speed`: 1000 or 100
pub fn diff_book_depth_stream(symbol: &str, update_speed: u16) -> String { format!("{symbol}@depth@{update_speed}ms") }

fn combined_stream(streams: Vec<String>) -> String { streams.join("/") }

// 定义一个枚举来表示不同类型的 WebSocket 连接
pub enum WebSocketConnection {
    Direct(WebSocketStream<MaybeTlsStream<TcpStream>>, Response),
    Proxies(WebSocketStream<MaybeTlsStream<Socks5Stream<tokio::net::TcpStream>>>, Response),
}

const WSS_PROXY_ENV_KEY: &str = "WSS_PROXY";

pub struct WebSockets<'a, WE> {
    //pub socket: Option<(WebSocketStream<MaybeTlsStream<S>>, Response)>,
    pub socket: Option<WebSocketConnection>,
    handler: Box<dyn FnMut(WE) -> Result<()> + 'a + Send>,
    conf: Config,
}

impl<'a, WE: serde::de::DeserializeOwned> WebSockets<'a, WE> {
    /// New websocket holder with default configuration
    /// # Examples
    /// see examples/binance_websockets.rs
    pub fn new<Callback>(handler: Callback) -> WebSockets<'a, WE>
    where
        Callback: FnMut(WE) -> Result<()> + 'a + Send,
    {
        Self::new_with_options(handler, Config::default())
    }

    /// New websocket holder with provided configuration
    /// # Examples
    /// see examples/binance_websockets.rs
    pub fn new_with_options<Callback>(handler: Callback, conf: Config) -> WebSockets<'a, WE>
    where
        Callback: FnMut(WE) -> Result<()> + 'a + Send,
    {
        WebSockets {
            socket: None,
            handler: Box::new(handler),
            conf,
        }
    }

    /// Connect to multiple websocket endpoints
    /// N.B: WE has to be CombinedStreamEvent
    pub async fn connect_multiple(&mut self, endpoints: Vec<String>) -> Result<()> {
        let mut url = Url::parse(&self.conf.ws_endpoint)?;
        url.path_segments_mut()
            .map_err(|_| Error::UrlParserError(url::ParseError::RelativeUrlWithoutBase))?
            .push(STREAM_ENDPOINT);
        url.set_query(Some(&format!("streams={}", combined_stream(endpoints))));

        self.handle_connect(url).await
    }

    /// Connect to a websocket endpoint
    pub async fn connect(&mut self, endpoint: &str) -> Result<()> {
        let wss: String = format!("{}/{}/{}", self.conf.ws_endpoint, WS_ENDPOINT, endpoint);
        let url = Url::parse(&wss)?;

        self.handle_connect(url).await
    }
    async fn handle_connect(&mut self, url: Url) -> Result<()> {
        // 检查是否存在 WSS_PROXY 环境变量
        if let Ok(proxy_addr) = std::env::var(WSS_PROXY_ENV_KEY) {
            // 使用 fast_socks5 建立代理流
            let proxy_stream = Socks5Stream::connect(proxy_addr, url.host_str().unwrap().to_string(), url.port_or_known_default().unwrap(), Socks5Config::default()).await
                .map_err(|e| Error::Msg(format!("Error creating proxy stream: {e}")))?;

            // 用 proxy_stream 替换直接的 connect_async 调用
            match client_async_tls(url, proxy_stream).await {
                Ok((stream, response)) => {
                    // 使用 Proxied 枚举变体
                    self.socket = Some(WebSocketConnection::Proxies(stream, response));
                    Ok(())
                },
                Err(e) => Err(Error::Msg(format!("Error during handshake: {e}"))),
            }
        } else {
            match connect_async(url).await {
                Ok((stream, response)) => {
                    // 使用 Direct 枚举变体
                    self.socket = Some(WebSocketConnection::Direct(stream, response));
                    Ok(())
                },
                Err(e) => Err(Error::Msg(format!("Error during handshake {e}"))),
            }
        }
    }
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(ref mut connection) = self.socket {
            // 根据连接类型处理断开连接
            match connection {
                WebSocketConnection::Direct(ref mut socket, _) => {
                    socket.close(None).await?;
                },
                WebSocketConnection::Proxies(ref mut socket, _) => {
                    socket.close(None).await?;
                },
            }
            Ok(())
        } else {
            Err(Error::Msg("Not able to close the connection".to_string()))
        }
    }
    //pub fn socket(&self) -> &Option<(WebSocketStream<MaybeTlsStream<S>>, Response)> { &self.socket }
    pub fn socket(&self) -> Option<&WebSocketConnection> {
        self.socket.as_ref()
    }

    async fn process_message(&mut self, message: Message) -> Result<()> {
        match message {
            Message::Text(msg) => {
                if msg.is_empty() {
                    return Ok(());
                }
                let event: WE = from_str(msg.as_str())?;
                (self.handler)(event)?;
            }
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            Message::Close(e) => {
                return Err(Error::Msg(format!("Disconnected {e:?}")));
            }
        }
        Ok(())
    }
    pub async fn event_loop(&mut self, running: &AtomicBool) -> Result<()> {
        while running.load(Ordering::Relaxed) {
            if let Some(ref mut connection) = self.socket {
                // 获取 trait 对象
                match connection {
                    WebSocketConnection::Direct(ref mut socket, _) => {
                        if let Some(message) = socket.next().await {
                            self.process_message(message?).await?;
                        }
                    }
                    WebSocketConnection::Proxies(ref mut socket, _) => {
                        if let Some(message) = socket.next().await {
                            self.process_message(message?).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
