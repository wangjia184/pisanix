// Copyright 2022 SphereEx Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use regex::Regex;
use tokio::sync::Semaphore;

use crate::{
    config,
    err::{BoxError, PluginError},
    layer::{Layer, Service},
};

#[derive(Clone)]
pub struct LimitLayer {
    config: Option<Vec<config::Limit>>,
}

#[derive(Clone)]
pub struct LimitConfig {
    pub regex: String,
    pub limit: usize,
    pub duration: Duration,
}

/// `Limit` instance
#[derive(Debug, Clone)]
pub struct LimitInstance {
    regex: Regex,
    limit_size: usize,
    semaphore: Arc<Semaphore>,
    // If the first match, the timing starts to take effect,
    // and duration `duration`
    duration: Duration,
    start_at: Option<Instant>,
}

impl LimitLayer {
    pub fn new(config: Vec<config::Limit>) -> LimitLayer {
        LimitLayer { config: Some(config) }
    }

    pub fn with_opt(config: Option<Vec<config::Limit>>) -> LimitLayer {
        LimitLayer { config }
    }

    fn create_instances(&self) -> Option<Vec<LimitInstance>> {
        if let Some(config) = &self.config {
            let mut instances = Vec::with_capacity(config.len());
            for c in config {
                let regex = Regex::new(&c.regex).unwrap();
                let semaphore = Arc::new(Semaphore::new(c.limit as usize));
                instances.push(LimitInstance {
                    limit_size: c.limit as usize,
                    regex,
                    semaphore,
                    duration: c.duration,
                    start_at: None,
                });
            }
            return Some(instances);
        }

        None
    }
}

impl<S> Layer<S> for LimitLayer {
    type Service = Limit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        let instances = self.create_instances();
        let mut limit = Limit { inner, instances: None };

        if let Some(instances) = instances {
            limit.instances = Some(Arc::new(Mutex::new(instances)))
        }

        limit
    }
}

#[derive(Debug, Clone)]
pub struct Limit<S> {
    inner: S,
    instances: Option<Arc<Mutex<Vec<LimitInstance>>>>,
}

impl<S> Limit<S> {
    // If accquire success return true, otherwise return fasle
    // If the semaphore is acquired at the same time, the duration will be invalid
    fn is_allow(&mut self, input: &str) -> (Option<usize>, bool) {
        if let Some(instances) = &self.instances {
            let mut instances = instances.lock();
            for (idx, c) in instances.iter_mut().enumerate() {
                if !c.regex.is_match(input) {
                    continue;
                }

                if c.start_at.is_none() {
                    // first match, set start_at
                    c.start_at = Some(Instant::now());
                    let permit = c.semaphore.clone().try_acquire_owned();

                    if permit.is_err() {
                        return (Some(idx), false);
                    }
                    permit.unwrap().forget();
                    return (Some(idx), true);
                } else {
                    // duration has invalid, return true
                    if c.start_at.unwrap().elapsed() > c.duration {
                        // enter next loop, reinit `Semaphore` and `start_at`
                        c.start_at = None;
                        c.semaphore = Arc::new(Semaphore::new(c.limit_size));
                        return (None, true);
                    } else {
                        let permit = c.clone().semaphore.try_acquire_owned();
                        if permit.is_err() {
                            return (Some(idx), false);
                        }
                        permit.unwrap().forget();
                        return (Some(idx), true);
                    }
                }
            }
        }

        (None, true)
    }

    pub fn add_permits(&mut self, idx: usize) {
        let instances = self.instances.as_mut().unwrap().lock();
        instances[idx].semaphore.add_permits(1)
    }
}

impl<S, Input> Service<Input> for Limit<S>
where
    S: Service<Input>,
    Input: AsRef<str>,
    S::Error: Into<BoxError>,
{
    type Output = (Option<usize>, S::Output);
    type Error = BoxError;

    fn handle(&mut self, input: Input) -> Result<Self::Output, Self::Error> {
        let (idx, is_allow) = self.is_allow(input.as_ref());
        if is_allow {
            let res = self.inner.handle(input).map_err(Into::into);
            match res {
                Ok(out) => return Ok((idx, out)),
                Err(e) => return Err(e),
            }
        }

        Err(Box::new(PluginError::LimitPluginReject))
    }
}

#[cfg(test)]
mod test {
    use std::{
        thread::{self, sleep},
        time::Duration,
    };

    use super::LimitLayer;
    use crate::{
        config,
        err::PluginError,
        layer::{service_fn, Service, ServiceBuilder},
    };

    fn test_service(input: &str) -> Result<String, PluginError> {
        sleep(Duration::new(5, 0));
        Ok(input.to_string())
    }

    #[test]
    fn test_limit() {
        let config = vec![config::Limit {
            regex: String::from(r"[A-Za-z]+$"),
            limit: 3,
            duration: Duration::new(50, 0),
        }];

        let svc = service_fn(test_service);

        let wrap_svc = ServiceBuilder::new().with_layer(LimitLayer::new(config)).build(svc);

        let mut tasks = vec![];
        for _ in 0..5 {
            let mut svc = wrap_svc.clone();
            //sleep(Duration::new(3, 0));
            tasks.push(thread::spawn(move || svc.handle("abc")))
        }

        let mut count = 0;
        for t in tasks {
            let res = t.join();
            println!("{:?}", res);
            let res = res.unwrap();
            match res {
                Ok(_) => count += 1,
                Err(e) => {
                    let e = e.downcast::<PluginError>().unwrap();
                    assert_eq!(*e, PluginError::LimitPluginReject);
                }
            }
        }

        assert_eq!(count, 3)
    }
}
