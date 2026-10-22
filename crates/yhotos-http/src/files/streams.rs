use crate::error::HttpError;
use axum::body::Body;
use http_body_util::BodyExt;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};

const MAX_ACTIVE_IPS: usize = 16_384;

pub(super) struct Streams {
    active: Mutex<HashMap<IpAddr, usize>>,
    cap: usize,
}
impl Streams {
    pub(super) fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(HashMap::new()),
            cap,
        })
    }
    pub(super) fn acquire(self: &Arc<Self>, ip: IpAddr) -> Result<Permit, HttpError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| HttpError::Internal(anyhow::anyhow!("stream accounting unavailable")))?;
        if active.get(&ip).copied().unwrap_or(0) >= self.cap
            || (!active.contains_key(&ip) && active.len() >= MAX_ACTIVE_IPS)
        {
            return Err(HttpError::RateLimited);
        }
        *active.entry(ip).or_default() += 1;
        Ok(Permit {
            streams: self.clone(),
            ip,
        })
    }
}
pub(super) struct Permit {
    streams: Arc<Streams>,
    ip: IpAddr,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.streams.active.lock()
            && let Some(count) = active.get_mut(&self.ip)
        {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.ip);
            }
        }
    }
}
pub(super) fn guard(body: Body, permit: Permit) -> Body {
    Body::new(body.map_frame(move |frame| {
        // The closure owns the permit until completion, error or cancellation
        // drops the response body. Headers alone never release the slot.
        let _keep_alive = &permit;
        frame
    }))
}
