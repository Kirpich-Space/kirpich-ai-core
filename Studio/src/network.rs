use anyhow::{Context, Result, bail};
use reqwest::Client;
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use url::Url;
#[derive(Clone, Debug)]
pub struct LocalEndpoint(Url);
impl LocalEndpoint {
    pub fn parse(s: &str) -> Result<Self> {
        let mut url = Url::parse(s).context("Некорректный адрес локального сервера")?;
        if url.scheme() != "http"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!(
                "Разрешён только http://127.0.0.1:порт или http://[::1]:порт без пути и учётных данных"
            );
        }
        let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
        if !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
            bail!("Разрешены только числовые loopback-адреса");
        }
        url.set_path("/");
        Ok(Self(url))
    }
    pub fn route(&self, path: &str) -> Url {
        let mut u = self.0.clone();
        u.set_path(path);
        u
    }
    pub fn segment(&self, base: &str, id: &str) -> Url {
        let mut u = self.route(base);
        u.path_segments_mut()
            .expect("HTTP base")
            .pop_if_empty()
            .push(id);
        u
    }
}
pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(600))
        .build()?)
}
#[derive(Clone, Default)]
pub struct Cancel(pub Arc<AtomicBool>);
impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
pub async fn json_response(r: reqwest::Response) -> Result<serde_json::Value> {
    let status = r.status();
    if !status.is_success() {
        bail!(
            "Локальный API: HTTP {}. Проверьте адрес, доступность и авторизацию.",
            status.as_u16()
        );
    }
    let mut stream = r.bytes_stream();
    let mut data = vec![];
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if data.len() + chunk.len() > 16 * 1024 * 1024 {
            bail!("Ответ API превышает 16 МиБ");
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).context("API вернул некорректный JSON")
}
