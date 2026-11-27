// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

package atlas

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// write sends a mutating request and normalizes 202 (job) and 200 (done) replies.
func (c *Client) write(ctx context.Context, method, path string, q url.Values, body any) (Accepted, error) {
	var raw json.RawMessage
	status, err := c.do(ctx, method, path, q, body, &raw)
	if err != nil {
		return Accepted{}, err
	}
	var a Accepted
	if status == http.StatusAccepted {
		if err := json.Unmarshal(raw, &a); err != nil {
			return Accepted{}, fmt.Errorf("atlas: decode accepted %s %s: %w", method, path, err)
		}
		if a.JobID == "" {
			return Accepted{}, fmt.Errorf("atlas: %s %s: 202 without job_id", method, path)
		}
		return a, nil
	}
	if len(raw) > 0 {
		if err := json.Unmarshal(raw, &a.Resource); err != nil {
			return Accepted{}, fmt.Errorf("atlas: decode %s %s: %w", method, path, err)
		}
	}
	a.Done = true
	a.State = JobSucceeded
	return a, nil
}

func esc(s string) string { return url.PathEscape(s) }

// ListVolumes calls GET /volumes. Non-admin callers only ever see their tenant.
func (c *Client) ListVolumes(ctx context.Context, f VolumeFilter) ([]Volume, error) {
	q := url.Values{}
	for k, v := range map[string]string{"state": f.State, "tenant": f.Tenant, "backend": f.Backend, "kind": f.Kind} {
		if v != "" {
			q.Set(k, v)
		}
	}
	var out []Volume
	_, err := c.do(ctx, http.MethodGet, APIPrefix+"/volumes", q, nil, &out)
	return out, err
}

// GetVolume calls GET /volumes/{id}. A missing volume returns an error for which IsNotFound is true.
func (c *Client) GetVolume(ctx context.Context, id string) (Volume, error) {
	var v Volume
	_, err := c.do(ctx, http.MethodGet, APIPrefix+"/volumes/"+esc(id), nil, nil, &v)
	return v, err
}

// CreateVolume calls POST /volumes (PVC-backed, any backend). Idempotent on
// (tenant_id, name, size_bytes). Resource.VolumeID is set on success.
func (c *Client) CreateVolume(ctx context.Context, r CreateVolumeRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/volumes", nil, r)
}

// DeleteVolume calls DELETE /volumes/{id}.
func (c *Client) DeleteVolume(ctx context.Context, id string) (Accepted, error) {
	return c.write(ctx, http.MethodDelete, APIPrefix+"/volumes/"+esc(id), nil, nil)
}

// ExpandVolume calls POST /volumes/{id}/expand. newSizeBytes must exceed the current size.
func (c *Client) ExpandVolume(ctx context.Context, id string, newSizeBytes int64) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/volumes/"+esc(id)+"/expand", nil,
		map[string]int64{"new_size_bytes": newSizeBytes})
}

// CreateRBDImage calls POST /rbd-images: a raw Ceph RBD image with no PVC,
// for consumers that open RBD directly (QEMU). Resource.VolumeID is
// "vol_{pool}_{image}" and Resource.RBD is "pool/image".
func (c *Client) CreateRBDImage(ctx context.Context, r CreateRBDImageRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/rbd-images", nil, r)
}

// DeleteRBDImage calls DELETE /rbd-images/{pool}/{image}.
func (c *Client) DeleteRBDImage(ctx context.Context, pool, image string) (Accepted, error) {
	return c.write(ctx, http.MethodDelete, APIPrefix+"/rbd-images/"+esc(pool)+"/"+esc(image), nil, nil)
}

// CreateSnapshot calls POST /volumes/{id}/snapshots. Resource.SnapshotID is set on success.
func (c *Client) CreateSnapshot(ctx context.Context, volumeID string, r SnapshotRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/volumes/"+esc(volumeID)+"/snapshots", nil, r)
}

// ListSnapshots calls GET /snapshots.
func (c *Client) ListSnapshots(ctx context.Context) ([]Snapshot, error) {
	var out []Snapshot
	_, err := c.do(ctx, http.MethodGet, APIPrefix+"/snapshots", nil, nil, &out)
	return out, err
}

// DeleteSnapshot calls DELETE /snapshots/{id}. Without force, a snapshot that
// volumes were cloned from is refused with a 409 (see IsConflict).
func (c *Client) DeleteSnapshot(ctx context.Context, id string, force bool) (Accepted, error) {
	var q url.Values
	if force {
		q = url.Values{"force": {"true"}}
	}
	return c.write(ctx, http.MethodDelete, APIPrefix+"/snapshots/"+esc(id), q, nil)
}

// CloneSnapshot calls POST /snapshots/{id}/clone (independent new volume; Name required).
func (c *Client) CloneSnapshot(ctx context.Context, id string, r CloneRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/snapshots/"+esc(id)+"/clone", nil, r)
}

// RestoreSnapshot calls POST /snapshots/{id}/restore (point-in-time copy as a new volume).
func (c *Client) RestoreSnapshot(ctx context.Context, id string, r CloneRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/snapshots/"+esc(id)+"/restore", nil, r)
}

// CreateBackup calls POST /backup-jobs. Resource.BackupID is set on success.
func (c *Client) CreateBackup(ctx context.Context, r BackupRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/backup-jobs", nil, r)
}

// CreateRestore calls POST /restore-jobs. Resource.VolumeID is the new volume.
func (c *Client) CreateRestore(ctx context.Context, r RestoreRequest) (Accepted, error) {
	return c.write(ctx, http.MethodPost, APIPrefix+"/restore-jobs", nil, r)
}

// ListBackups calls GET /backups, scoped to one volume when volumeID is non-empty.
func (c *Client) ListBackups(ctx context.Context, volumeID string) ([]Backup, error) {
	var q url.Values
	if volumeID != "" {
		q = url.Values{"volume_id": {volumeID}}
	}
	var out []Backup
	_, err := c.do(ctx, http.MethodGet, APIPrefix+"/backups", q, nil, &out)
	return out, err
}

// GetJob calls GET /jobs/{id}.
func (c *Client) GetJob(ctx context.Context, id string) (Job, error) {
	var j Job
	_, err := c.do(ctx, http.MethodGet, APIPrefix+"/jobs/"+esc(id), nil, nil, &j)
	return j, err
}

// JobFailedError is returned by Wait when the job ends in the failed state.
type JobFailedError struct{ Job Job }

func (e *JobFailedError) Error() string {
	msg := "no error message"
	if e.Job.Error != nil && strings.TrimSpace(*e.Job.Error) != "" {
		msg = *e.Job.Error
	}
	return fmt.Sprintf("atlas: job %s (%s) failed: %s", e.Job.ID, e.Job.JobType, msg)
}

// IsJobFailed reports whether err came from a job that ended in the failed state.
func IsJobFailed(err error) bool {
	var jf *JobFailedError
	return errors.As(err, &jf)
}

// Wait polls a write's job until it succeeds, fails, or ctx ends. A write
// that completed synchronously returns immediately. interval <= 0 uses 1s.
// Callers must not treat a write as done before Wait returns nil: a 202 only
// means the job was queued.
func (c *Client) Wait(ctx context.Context, a Accepted, interval time.Duration) (Job, error) {
	if a.Done {
		return Job{State: JobSucceeded}, nil
	}
	return c.WaitJob(ctx, a.JobID, interval)
}

// WaitJob polls GET /jobs/{id} until the job is terminal or ctx ends.
func (c *Client) WaitJob(ctx context.Context, id string, interval time.Duration) (Job, error) {
	if interval <= 0 {
		interval = time.Second
	}
	t := time.NewTicker(interval)
	defer t.Stop()
	for {
		j, err := c.GetJob(ctx, id)
		if err != nil {
			return j, err
		}
		switch j.State {
		case JobSucceeded:
			return j, nil
		case JobFailed:
			return j, &JobFailedError{Job: j}
		}
		select {
		case <-ctx.Done():
			return j, fmt.Errorf("atlas: waiting for job %s (state %s, %s%%): %w",
				id, j.State, strconv.FormatInt(j.ProgressPercent, 10), ctx.Err())
		case <-t.C:
		}
	}
}
