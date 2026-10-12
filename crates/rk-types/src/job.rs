//! Finite, indexed batches. An index identifies one experiment across retries.
use crate::workloads::PodTemplateSpec;
use crate::{ObjectMeta, Resource, ResourceKind, RestartPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const EVICTED_ATTEMPT_MESSAGE: &str = "Job attempt evicted; do not restart";

fn kind_job() -> String {
    "Job".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    #[serde(default = "crate::meta::api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_job")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: JobSpec,
    #[serde(default)]
    pub status: JobStatus,
}
impl Resource for Job {
    const KIND: ResourceKind = ResourceKind::Job;
    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobSpec {
    pub completions: u32,
    pub parallelism: u32,
    /// Retries allowed per index after its initial attempt.
    pub backoff_limit: u32,
    pub template: PodTemplateSpec,
}
impl Default for JobSpec {
    fn default() -> Self {
        let mut template = PodTemplateSpec::default();
        template.spec.restart_policy = RestartPolicy::Never;
        Self {
            completions: 1,
            parallelism: 1,
            backoff_limit: 3,
            template,
        }
    }
}
impl JobSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.completions == 0 || self.completions > 100_000 {
            return Err("Job completions must be between 1 and 100000".into());
        }
        if self.parallelism == 0 || self.parallelism > self.completions {
            return Err("Job parallelism must be between 1 and completions".into());
        }
        if self.backoff_limit > 100 {
            return Err("Job backoffLimit must not exceed 100".into());
        }
        if self.template.spec.restart_policy != RestartPolicy::Never {
            return Err(
                "Job template restartPolicy must be Never; the Job controls retries".into(),
            );
        }
        if self.template.spec.containers.len() != 1 {
            return Err("Job template must contain exactly one container".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobPhase {
    #[default]
    Running,
    Complete,
    Failed,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobStatus {
    pub phase: JobPhase,
    pub active: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub completed_indexes: BTreeSet<u32>,
    pub failed_attempts: BTreeMap<u32, BTreeSet<u32>>,
}
