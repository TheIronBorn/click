// Copyright 2017 Databricks, Inc.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at

// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Dealing with various kubernetes api calls

use chrono::DateTime;
use chrono::offset::utc::UTC;
use hyper::{Client,Url};
use hyper::client::request::Request;
use hyper::client::response::Response;
use hyper::header::{Authorization, Bearer};
use hyper::method::Method;
use hyper::net::HttpsConnector;
use hyper::status::StatusCode;
use hyper_rustls::TlsClient;
use serde::Deserialize;
use serde_json;
use serde_json::Value;
use rustls::{Certificate, PrivateKey};

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use error::{KubeErrNo, KubeError};

// Various things we can return from the kubernetes api

// objects
#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub name: String,
    pub namespace: Option<String>,
    #[serde(rename="creationTimestamp")]
    pub creation_timestamp: Option<DateTime<UTC>>,
}

// pods

#[derive(Debug, Deserialize)]
pub struct PodStatus {
    pub phase: String,
}


#[derive(Debug, Deserialize)]
pub struct Pod {
    pub metadata: Metadata,
    pub status: PodStatus,
}

#[derive(Debug, Deserialize)]
pub struct PodList {
    pub items: Vec<Pod>,
}

// Events
#[derive(Debug, Deserialize)]
pub struct Event {
    pub count: u32,
    pub message: String,
    pub reason: String,
    #[serde(rename="lastTimestamp")]
    pub last_timestamp: DateTime<UTC>,
}

#[derive(Debug, Deserialize)]
pub struct EventList {
    pub items: Vec<Event>,
}



// Nodes
#[derive(Debug, Deserialize)]
pub struct NodeCondition {
    #[serde(rename="type")]
    pub typ: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct NodeStatus {
    pub conditions: Vec<NodeCondition>,
}

#[derive(Debug, Deserialize)]
pub struct NodeSpec {
    pub unschedulable: Option<bool>,
}


#[derive(Debug, Deserialize)]
pub struct Node {
    pub metadata: Metadata,
    pub spec: NodeSpec,
    pub status: NodeStatus,
}

#[derive(Debug, Deserialize)]
pub struct NodeList {
    pub items: Vec<Node>,
}

// Auth is either a token or a cert and key
pub enum KlusterAuth {
    Token(String),
    CertKey(Vec<Certificate>, PrivateKey),
}

impl KlusterAuth {
    pub fn with_token(token: &str) -> KlusterAuth {
        KlusterAuth::Token(token.to_owned())
    }

    pub fn with_cert_and_key(cert: Certificate, private_key: PrivateKey) -> KlusterAuth {
        KlusterAuth::CertKey(vec!(cert), private_key)
    }
}

pub struct Kluster {
    pub name: String,
    endpoint: Url,
    auth: KlusterAuth,
    cert_path: String,
    client: Client,
}

impl Kluster {

    fn make_tlsclient(cert_path: &str, auth: &KlusterAuth) -> TlsClient {
        let mut tlsclient = TlsClient::new();
        {
            // add the cert to the root store
            let mut cfg = Arc::get_mut(&mut tlsclient.cfg).unwrap();
            let f = File::open(cert_path).unwrap();
            let mut br = BufReader::new(f);
            let added = cfg.root_store.add_pem_file(&mut br).unwrap();
            if added.1 > 0 {
                println!("[WARNING] Couldn't add some certs from {}", cert_path);
            }

            if let &KlusterAuth::CertKey(ref cert, ref key) = auth {
                cfg.set_single_client_cert(cert.clone(),key.clone());
            }
        }
        tlsclient
    }

    pub fn new(name: &str, cert_path: &str, server: &str, auth: KlusterAuth) -> Result<Kluster, KubeError> {
        let tlsclient = Kluster::make_tlsclient(cert_path, &auth);
        Ok(Kluster {
            name: name.to_owned(),
            endpoint: try!(Url::parse(server)),
            auth: auth,
            cert_path: cert_path.to_owned(),
            client: Client::with_connector(HttpsConnector::new(tlsclient)),
        })
    }

    fn send_req(&self, path: &str) -> Result<Response, KubeError> {
        let url = try!(self.endpoint.join(path));
        let req = self.client.get(url);
        let req =
            if let KlusterAuth::Token(ref token) = self.auth {
                req.header(Authorization(
                    Bearer {
                        token: token.clone()
                    }))
            } else {
                req
            };
        req.send().map_err(|he| KubeError::from(he))
    }

    fn check_resp(&self, resp: Response) -> Result<Response, KubeError> {
        if resp.status == StatusCode::Ok {
            Ok(resp)
        } else if resp.status == StatusCode::Unauthorized {
            Err(KubeError::Kube(KubeErrNo::Unauthorized))
        } else {
            Err(KubeError::Kube(KubeErrNo::Unknown))
        }
    }

    /// Get a resource and deserialize it as a T
    pub fn get<T>(&self, path: &str) -> Result<T, KubeError>
        where T: Deserialize {

        let resp = try!(self.send_req(path));
        let resp = try!(self.check_resp(resp));
        serde_json::from_reader(resp).map_err(|sje| KubeError::from(sje))
    }

    /// Get a Response.  Response implements Read, so this allows for a streaming read (for things
    /// like printing logs)
    pub fn get_read(&self, path: &str, timeout: Option<Duration>) -> Result<Response, KubeError> {
        if timeout.is_some() {
            let url = try!(self.endpoint.join(path));
            let mut req = try!(Request::with_connector(Method::Get,
                                                       url,
                                                       &HttpsConnector::new(
                                                           Kluster::make_tlsclient(self.cert_path.as_str(), &self.auth)
                                                       )));
            { // scope for mutable borrow of req
                let mut headers = req.headers_mut();
                if let KlusterAuth::Token(ref token) = self.auth {
                    headers.set(Authorization(
                        Bearer {
                            token: token.clone()
                        }
                    ));
                }
            }
            try!(req.set_read_timeout(timeout));
            let next = try!(req.start());
            let resp = try!(next.send().map_err(|he| KubeError::from(he)));
            self.check_resp(resp)
        } else {
            let resp = try!(self.send_req(path));
            self.check_resp(resp)
        }
    }

    /// Get a serde_json::Value
    pub fn get_value(&self, path: &str) -> Result<Value, KubeError> {
        let resp = try!(self.send_req(path));
        let resp = try!(self.check_resp(resp));
        serde_json::from_reader(resp).map_err(|sje| KubeError::from(sje))
    }

    /// Issue an HTTP DELETE request to the specified path
    pub fn delete(&self, path: &str) -> Result<Response, KubeError> {
        let url = try!(self.endpoint.join(path));
        let req = self.client.delete(url);
        let req =
            if let KlusterAuth::Token(ref token) = self.auth {
                req.header(Authorization(
                    Bearer {
                        token: token.clone()
                    }))
            } else {
                req
            };
        req.send().map_err(|he| KubeError::from(he))
    }
}
