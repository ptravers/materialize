// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Management of user secrets via Kubernetes.

use std::iter;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use kube::api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams};
use mz_repr::CatalogItemId;
use mz_secrets::{SecretsController, SecretsReader};

use crate::{FIELD_MANAGER, KubernetesOrchestrator, util};

#[async_trait]
impl SecretsController for KubernetesOrchestrator {
    async fn ensure(&self, id: CatalogItemId, contents: &[u8]) -> Result<(), anyhow::Error> {
        let name = secret_name(id, &self.config.name_prefix());
        let data = iter::once(("contents".into(), ByteString(contents.into())));
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            data: Some(data.collect()),
            ..Default::default()
        };
        self.secret_api
            .patch(
                &name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(secret),
            )
            .await?;
        Ok(())
    }

    async fn delete(&self, id: CatalogItemId) -> Result<(), anyhow::Error> {
        // We intentionally don't wait for the secret to be deleted; our
        // obligation is only to initiate the deletion. Garbage collecting
        // secrets that fail to delete will be the responsibility of a future
        // garbage collection task.
        match self
            .secret_api
            .delete(
                &secret_name(id, &self.config.name_prefix()),
                &DeleteParams::default(),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Secret is already deleted.
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => return Err(e.into()),
        }
    }

    async fn list(&self) -> Result<Vec<CatalogItemId>, anyhow::Error> {
        let objs = self.secret_api.list(&ListParams::default()).await?;
        let mut ids = Vec::new();
        for item in objs.items {
            // Ignore unnamed objects.
            let Some(name) = item.metadata.name else {
                continue;
            };
            // Ignore invalidly named objects.
            let Some(id) = from_secret_name(&name, &self.config.name_prefix()) else {
                continue;
            };
            ids.push(id);
        }
        Ok(ids)
    }

    fn reader(&self) -> Arc<dyn SecretsReader> {
        Arc::new(KubernetesSecretsReader {
            secret_api: self.secret_api.clone(),
            name_prefix: self.config.name_prefix(),
        })
    }
}

/// Reads secrets managed by a [`KubernetesOrchestrator`].
#[derive(Debug)]
pub struct KubernetesSecretsReader {
    secret_api: Api<Secret>,
    name_prefix: String,
}

impl KubernetesSecretsReader {
    /// Constructs a new Kubernetes secrets reader.
    ///
    /// The `context` parameter works like
    /// [`KubernetesOrchestratorConfig::context`](crate::KubernetesOrchestratorConfig::context).
    pub async fn new(
        context: String,
        name_prefix: Option<String>,
    ) -> Result<KubernetesSecretsReader, anyhow::Error> {
        let (client, _) = util::create_client(context).await?;
        let secret_api: Api<Secret> = Api::default_namespaced(client);
        let name_prefix = name_prefix.clone().unwrap_or_default();
        Ok(KubernetesSecretsReader {
            secret_api,
            name_prefix,
        })
    }
}

#[async_trait]
impl SecretsReader for KubernetesSecretsReader {
    async fn read(&self, id: CatalogItemId) -> Result<Vec<u8>, anyhow::Error> {
        let secret = self
            .secret_api
            .get(&secret_name(id, &self.name_prefix))
            .await?;
        let mut data = secret
            .data
            .ok_or_else(|| anyhow!("internal error: secret missing data field"))?;
        let contents = data
            .remove("contents")
            .ok_or_else(|| anyhow!("internal error: secret missing contents field"))?;
        Ok(contents.0)
    }
}

const SECRET_NAME_PREFIX: &str = "user-managed-";

fn secret_name(id: CatalogItemId, name_prefix: &str) -> String {
    format!("{name_prefix}{SECRET_NAME_PREFIX}{id}")
}

fn from_secret_name(name: &str, name_prefix: &str) -> Option<CatalogItemId> {
    name.strip_prefix(&name_prefix)
        .and_then(|name| name.strip_prefix(SECRET_NAME_PREFIX))
        .and_then(|id| id.parse().ok())
}
