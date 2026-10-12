//! Pod: the unit of scheduling. One or more containers sharing network and
//! lifecycle, placed on a single node.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Resource;
use crate::meta::{ObjectMeta, ResourceKind, api_version_v1};
use crate::quantity::Resources;

fn kind_pod() -> String {
    "Pod".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    #[serde(default = "api_version_v1")]
    pub api_version: String,
    #[serde(default = "kind_pod")]
    pub kind: String,
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: PodSpec,
    #[serde(default)]
    pub status: PodStatus,
}

impl Resource for Pod {
    const KIND: ResourceKind = ResourceKind::Pod;

    fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PodSpec {
    pub containers: Vec<Container>,
    /// Node labels this pod requires; honored by the scheduler from Phase 4.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Set by the scheduler (the "binding"). A pod with `nodeName` unset is
    /// unscheduled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub restart_policy: RestartPolicy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[default]
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub name: String,
    pub image: String,
    /// Overrides the image entrypoint when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<ContainerPort>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<VolumeMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_probe: Option<Probe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_probe: Option<Probe>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<EnvVarSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<KeyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<KeyRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub container_port: u16,
    #[serde(default)]
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    #[default]
    #[serde(rename = "TCP")]
    Tcp,
    #[serde(rename = "UDP")]
    Udp,
}

fn default_probe_period() -> u32 {
    10
}

fn default_probe_timeout() -> u32 {
    1
}

fn default_probe_failure_threshold() -> u32 {
    3
}

/// Liveness/readiness probe. Exactly one of `httpGet`, `tcpSocket` or `exec`
/// should be set; the API server validates this from Phase 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_get: Option<HttpGetAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_socket: Option<TcpSocketAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecAction>,
    #[serde(default)]
    pub initial_delay_seconds: u32,
    #[serde(default = "default_probe_period")]
    pub period_seconds: u32,
    #[serde(default = "default_probe_timeout")]
    pub timeout_seconds: u32,
    #[serde(default = "default_probe_failure_threshold")]
    pub failure_threshold: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpGetAction {
    #[serde(default = "default_http_path")]
    pub path: String,
    pub port: u16,
}

fn default_http_path() -> String {
    "/".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcpSocketAction {
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecAction {
    pub command: Vec<String>,
}

/// A named volume; exactly one source should be set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map: Option<ConfigMapVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<HostPathVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_dir: Option<EmptyDirVolumeSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapVolumeSource {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretVolumeSource {
    pub secret_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPathVolumeSource {
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmptyDirVolumeSource {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeMount {
    /// Must match the name of a volume in the pod's `spec.volumes`.
    pub name: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PodStatus {
    pub phase: PodPhase,
    /// Kubernetes spells the acronym in full caps — `podIP`, not the
    /// `podIp` that camelCase renaming would produce.
    #[serde(rename = "podIP", skip_serializing_if = "Option::is_none")]
    pub pod_ip: Option<Ipv4Addr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PodPhase {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerStatus {
    pub name: String,
    pub ready: bool,
    pub restart_count: u32,
    pub state: ContainerState,
}

/// Mirrors the Kubernetes shape: at most one of the three is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<ContainerStateWaiting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<ContainerStateRunning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminated: Option<ContainerStateTerminated>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerStateWaiting {
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerStateRunning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerStateTerminated {
    pub exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
apiVersion: v1
kind: Pod
metadata:
  name: web-0
  namespace: default
  labels:
    app: web
spec:
  containers:
    - name: nginx
      image: nginx:1.27
      ports:
        - containerPort: 80
      env:
        - name: MODE
          value: production
        - name: API_KEY
          valueFrom:
            secretKeyRef:
              name: web-secrets
              key: apiKey
      resources:
        requests:
          cpu: 250m
          memory: 64Mi
        limits:
          cpu: "1"
          memory: 256Mi
      livenessProbe:
        httpGet:
          path: /healthz
          port: 80
        initialDelaySeconds: 3
  nodeSelector:
    disk: ssd
  restartPolicy: Always
  volumes:
    - name: config
      configMap:
        name: web-config
"#;

    #[test]
    fn pod_manifest_parses_with_kubernetes_field_names() {
        let pod: Pod = serde_yaml::from_str(MANIFEST).unwrap();
        assert_eq!(pod.kind, "Pod");
        assert_eq!(pod.metadata.name, "web-0");
        assert_eq!(pod.spec.containers.len(), 1);

        let container = &pod.spec.containers[0];
        assert_eq!(container.image, "nginx:1.27");
        assert_eq!(container.ports[0].container_port, 80);
        assert_eq!(container.resources.requests.cpu.unwrap().as_millis(), 250);
        assert_eq!(
            container.resources.limits.memory.unwrap().as_bytes(),
            256 << 20
        );

        let probe = container.liveness_probe.as_ref().unwrap();
        assert_eq!(probe.http_get.as_ref().unwrap().path, "/healthz");
        assert_eq!(probe.period_seconds, 10, "default applies");

        assert_eq!(pod.spec.node_selector["disk"], "ssd");
        assert_eq!(pod.spec.restart_policy, RestartPolicy::Always);
        assert!(pod.spec.node_name.is_none(), "unscheduled until bound");
        assert_eq!(pod.status.phase, PodPhase::Pending);
    }

    #[test]
    fn pod_round_trips_through_yaml() {
        let pod: Pod = serde_yaml::from_str(MANIFEST).unwrap();
        let yaml = serde_yaml::to_string(&pod).unwrap();
        let back: Pod = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(pod, back);
    }

    #[test]
    fn minimal_pod_gets_defaults() {
        let pod: Pod = serde_yaml::from_str(
            "metadata:\n  name: tiny\nspec:\n  containers:\n    - name: c\n      image: alpine\n",
        )
        .unwrap();
        assert_eq!(pod.api_version, "v1");
        assert_eq!(pod.kind, "Pod");
        assert_eq!(pod.spec.restart_policy, RestartPolicy::Always);
        assert!(pod.spec.containers[0].resources.requests.is_empty());
    }
}

/// Validation shared by API admission and the node agent. Only directory
/// hostPath and emptyDir volumes are implemented; fail explicitly otherwise.
pub fn validate_volumes(spec: &PodSpec) -> Result<(), String> {
    use std::collections::BTreeSet;
    let mut names = BTreeSet::new();
    for volume in &spec.volumes {
        let name = &volume.name;
        if name.is_empty()
            || name.len() > 63
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(
                "volume names must contain 1..63 lowercase letters, digits or hyphens".into(),
            );
        }
        if !names.insert(name.as_str()) {
            return Err(format!("duplicate volume {name}"));
        }
        let sources = usize::from(volume.host_path.is_some())
            + usize::from(volume.empty_dir.is_some())
            + usize::from(volume.config_map.is_some())
            + usize::from(volume.secret.is_some());
        if sources != 1 {
            return Err(format!("volume {name} must have exactly one source"));
        }
        if volume.config_map.is_some() || volume.secret.is_some() {
            return Err(
                "ConfigMap/Secret file volumes are not supported; use environment references"
                    .into(),
            );
        }
        if let Some(host) = &volume.host_path {
            validate_absolute_directory(&host.path)?;
        }
    }
    for container in &spec.containers {
        let mut paths = Vec::<&str>::new();
        for mount in &container.volume_mounts {
            if !names.contains(mount.name.as_str()) {
                return Err(format!("unknown volume {}", mount.name));
            }
            validate_mount_path(&mount.mount_path)?;
            let path = std::path::Path::new(&mount.mount_path);
            if paths.iter().any(|previous| {
                path.starts_with(previous) || std::path::Path::new(previous).starts_with(path)
            }) {
                return Err("duplicate or nested volume mount paths are not supported".into());
            }
            paths.push(&mount.mount_path);
        }
    }
    Ok(())
}

pub fn validate_absolute_directory(path: &str) -> Result<(), String> {
    if !path.starts_with('/')
        || path == "/"
        || path.contains('\0')
        || path
            .split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(format!(
            "expected a normalized absolute directory path other than /: {path:?}"
        ));
    }
    Ok(())
}

pub fn validate_mount_path(path: &str) -> Result<(), String> {
    validate_absolute_directory(path)?;
    if ["/proc", "/sys", "/dev"]
        .iter()
        .any(|p| std::path::Path::new(path).starts_with(p))
    {
        return Err("mount paths under /proc, /sys and /dev are reserved".into());
    }
    Ok(())
}

#[cfg(test)]
mod volume_tests {
    use super::*;
    fn spec() -> PodSpec {
        serde_yaml::from_str("containers:\n- name: sim\n  image: sim\n  volumeMounts:\n  - name: results\n    mountPath: /results\nvolumes:\n- name: results\n  hostPath:\n    path: /srv/results\n").unwrap()
    }
    #[test]
    fn validates_references_sources_and_paths() {
        assert!(validate_volumes(&spec()).is_ok());
        let mut s = spec();
        s.volumes[0].empty_dir = Some(EmptyDirVolumeSource {});
        assert!(validate_volumes(&s).is_err());
        let mut s = spec();
        s.containers[0].volume_mounts[0].name = "missing".into();
        assert!(validate_volumes(&s).is_err());
        for path in [
            "/", "relative", "/a/../b", "/a/./b", "/a//b", "/dev/shm", "/proc/a", "/sys",
        ] {
            let mut s = spec();
            s.containers[0].volume_mounts[0].mount_path = path.into();
            assert!(validate_volumes(&s).is_err(), "{path}");
        }
        let mut s = spec();
        let mut nested = s.containers[0].volume_mounts[0].clone();
        nested.mount_path = "/results/nested".into();
        s.containers[0].volume_mounts.push(nested);
        assert!(validate_volumes(&s).is_err());
    }
}
