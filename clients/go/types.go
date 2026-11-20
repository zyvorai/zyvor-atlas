// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

package atlas

import (
	"encoding/json"
	"strings"
)

// Wire types mirror crates/atlas-api-types/src/lib.rs. Optional Rust fields
// (Option<T>) are pointers or omitempty here.

// Version is GET /version.
type Version struct {
	Name    string `json:"name"`
	Version string `json:"version"`
	API     string `json:"api"`
}

// Volume kinds.
const (
	KindBlock      = "block"
	KindFilesystem = "filesystem"
	KindObject     = "object"
)

// Volume is a StorageVolume.
type Volume struct {
	ID                  string  `json:"id"`
	ClusterID           *string `json:"cluster_id"`
	PoolID              *string `json:"pool_id"`
	Name                string  `json:"name"`
	Kind                string  `json:"kind"`
	BackendNativeID     *string `json:"backend_native_id"`
	SizeBytes           int64   `json:"size_bytes"`
	UsedBytes           *int64  `json:"used_bytes"`
	State               string  `json:"state"`
	Health              string  `json:"health"`
	KubernetesNamespace *string `json:"kubernetes_namespace"`
	PVCName             *string `json:"pvc_name"`
	StorageClassName    *string `json:"storage_class_name"`
}

// NativeID returns BackendNativeID or "" when unset.
func (v Volume) NativeID() string {
	if v.BackendNativeID == nil {
		return ""
	}
	return *v.BackendNativeID
}

// ParseRBD splits a Ceph RBD backend_native_id ("rbd:pool/image" from the
// direct-RBD API, or "pool/image" from CSI discovery) into pool and image.
func ParseRBD(nativeID string) (pool, image string, ok bool) {
	s := strings.TrimPrefix(nativeID, "rbd:")
	pool, image, found := strings.Cut(s, "/")
	if !found || pool == "" || image == "" || strings.ContainsAny(s, ":@") || strings.Contains(image, "/") {
		return "", "", false
	}
	return pool, image, true
}

// Owner records which product resource owns a volume (product_bindings).
type Owner struct {
	Product      string `json:"product"`
	ResourceType string `json:"resource_type"`
	ResourceID   string `json:"resource_id"`
	// Role defaults to "data_disk" on the server when empty.
	Role string `json:"role,omitempty"`
}

// KubernetesOpts controls the PVC Atlas creates for a volume.
type KubernetesOpts struct {
	BackendID    string   `json:"backend_id,omitempty"`
	Namespace    string   `json:"namespace,omitempty"`
	CreatePVC    bool     `json:"create_pvc"`
	AccessModes  []string `json:"access_modes,omitempty"`
	VolumeMode   string   `json:"volume_mode,omitempty"`
	StorageClass string   `json:"storage_class,omitempty"`
}

// CreateVolumeRequest is the body of POST /volumes. Name must be an RFC 1123
// resource name; Policy, when set, must be a known intent.
type CreateVolumeRequest struct {
	TenantID   string          `json:"tenant_id"`
	Name       string          `json:"name"`
	SizeBytes  int64           `json:"size_bytes"`
	Kind       string          `json:"kind,omitempty"`
	Policy     string          `json:"policy,omitempty"`
	Pool       string          `json:"pool,omitempty"`
	Owner      *Owner          `json:"owner,omitempty"`
	Kubernetes *KubernetesOpts `json:"kubernetes,omitempty"`
}

// CreateRBDImageRequest is the body of POST /rbd-images (direct RBD, no PVC).
type CreateRBDImageRequest struct {
	Name      string `json:"name"`
	SizeBytes int64  `json:"size_bytes"`
	Pool      string `json:"pool,omitempty"`
	TenantID  string `json:"tenant_id,omitempty"`
}

// SnapshotRequest is the body of POST /volumes/{id}/snapshots.
type SnapshotRequest struct {
	Name          string `json:"name,omitempty"`
	SnapshotClass string `json:"snapshot_class,omitempty"`
}

// CloneRequest is the body of POST /snapshots/{id}/clone and /restore.
type CloneRequest struct {
	Name         string `json:"name,omitempty"`
	Namespace    string `json:"namespace,omitempty"`
	StorageClass string `json:"storage_class,omitempty"`
	SizeBytes    int64  `json:"size_bytes,omitempty"`
	Owner        *Owner `json:"owner,omitempty"`
}

// Backup modes.
const (
	BackupManifest = "manifest"
	BackupData     = "data"
)

// BackupRequest is the body of POST /backup-jobs.
type BackupRequest struct {
	VolumeID   string `json:"volume_id"`
	BucketID   string `json:"bucket_id"`
	Mode       string `json:"mode,omitempty"`
	Keep       int64  `json:"keep,omitempty"`
	MaxAgeSecs int64  `json:"max_age_secs,omitempty"`
}

// Restore modes.
const (
	RestoreSnapshot = "snapshot"
	RestoreData     = "data"
)

// RestoreRequest is the body of POST /restore-jobs.
type RestoreRequest struct {
	BackupID     string `json:"backup_id"`
	Name         string `json:"name,omitempty"`
	StorageClass string `json:"storage_class,omitempty"`
	Mode         string `json:"mode,omitempty"`
}

// Snapshot is a StorageSnapshot.
type Snapshot struct {
	ID               string  `json:"id"`
	TenantID         string  `json:"tenant_id"`
	VolumeID         string  `json:"volume_id"`
	Name             string  `json:"name"`
	BackendNativeID  *string `json:"backend_native_id"`
	Consistency      string  `json:"consistency"`
	State            string  `json:"state"`
	Protected        bool    `json:"protected"`
	ParentSnapshotID *string `json:"parent_snapshot_id"`
	CreatedAt        *string `json:"created_at"`
}

// Backup is a BackupRecord.
type Backup struct {
	ID         string  `json:"id"`
	TenantID   string  `json:"tenant_id"`
	VolumeID   string  `json:"volume_id"`
	SnapshotID *string `json:"snapshot_id"`
	BucketID   string  `json:"bucket_id"`
	ObjectKey  string  `json:"object_key"`
	Format     string  `json:"format"`
	Checksum   *string `json:"checksum"`
	State      string  `json:"state"`
	CreatedAt  *string `json:"created_at"`
}

// Job states (job_state in atlas-api-types).
const (
	JobPending   = "pending"
	JobQueued    = "queued"
	JobRunning   = "running"
	JobVerifying = "verifying"
	JobSucceeded = "succeeded"
	JobFailed    = "failed"
)

// Job is a JobRecord.
type Job struct {
	ID              string          `json:"id"`
	TenantID        string          `json:"tenant_id"`
	JobType         string          `json:"job_type"`
	State           string          `json:"state"`
	RequestedBy     string          `json:"requested_by"`
	ProgressPercent int64           `json:"progress_percent"`
	Error           *string         `json:"error"`
	Result          json.RawMessage `json:"result"`
	CreatedAt       *string         `json:"created_at"`
	UpdatedAt       *string         `json:"updated_at"`
}

// Terminal reports whether the job reached succeeded or failed.
func (j Job) Terminal() bool { return j.State == JobSucceeded || j.State == JobFailed }

// Accepted is the response to a write. Most writes return 202 with a job to
// poll; some complete synchronously with 200 (for example native-backend
// deletes and snapshots), in which case JobID is empty and Done is true.
// Resource carries the operation's ids (volume_id, snapshot_id, backup_id, ...).
type Accepted struct {
	JobID    string   `json:"job_id"`
	State    string   `json:"state"`
	Resource Resource `json:"resource"`
	Done     bool     `json:"-"`
}

// Resource holds the ids an accepted write reports. Fields not relevant to
// the operation stay empty.
type Resource struct {
	VolumeID     string `json:"volume_id,omitempty"`
	SnapshotID   string `json:"snapshot_id,omitempty"`
	BackupID     string `json:"backup_id,omitempty"`
	ObjectKey    string `json:"object_key,omitempty"`
	BucketID     string `json:"bucket_id,omitempty"`
	StorageClass string `json:"storage_class,omitempty"`
	Namespace    string `json:"namespace,omitempty"`
	PVC          string `json:"pvc,omitempty"`
	RBD          string `json:"rbd,omitempty"`
	BackendID    string `json:"backend_id,omitempty"`
	FromSnapshot string `json:"from_snapshot,omitempty"`
	FromBackup   string `json:"from_backup,omitempty"`
	Mode         string `json:"mode,omitempty"`
}

// VolumeFilter narrows GET /volumes. Empty fields are not sent.
type VolumeFilter struct {
	State   string
	Tenant  string
	Backend string
	Kind    string
}
