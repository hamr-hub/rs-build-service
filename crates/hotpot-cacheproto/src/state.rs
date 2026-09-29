//! 缓存协议路由状态。

use crate::remote::RemoteCache;

#[derive(Clone)]
pub struct CacheState {
    pub cache: RemoteCache,
}

impl CacheState {
    pub fn new(cache: RemoteCache) -> Self {
        Self { cache }
    }
}
