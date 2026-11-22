// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

package atlas

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

type fakeGateway struct {
	t     *testing.T
	mu    sync.Mutex
	polls int
	calls []string
}

func (f *fakeGateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	f.calls = append(f.calls, r.Method+" "+r.URL.RequestURI())
	f.mu.Unlock()
	if r.Header.Get("Authorization") != "Bearer tok" {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = io.WriteString(w, `{"error":{"code":"AUTH_ERROR","message":"missing bearer"}}`)
		return
	}
	body, _ := io.ReadAll(r.Body)
	switch {
	case r.Method == http.MethodGet && r.URL.Path == "/version":
		_, _ = io.WriteString(w, `{"name":"atlas-gateway","version":"0.4.0","api":"v1"}`)
	case r.Method == http.MethodPost && r.URL.Path == "/api/atlas/v1/volumes":
		var req CreateVolumeRequest
		if err := json.Unmarshal(body, &req); err != nil || req.Owner == nil || req.Owner.Product != "kairon" || !req.Kubernetes.CreatePVC {
			f.t.Errorf("bad create body: %s", body)
		}
		w.WriteHeader(http.StatusAccepted)
		_, _ = io.WriteString(w, `{"job_id":"job_1","state":"queued","resource":{"volume_id":"vol_1","storage_class":"zyvor-rbd-prod","namespace":"default","pvc":"web-root"},"links":{"job":"/api/atlas/v1/jobs/job_1"}}`)
	case r.Method == http.MethodGet && r.URL.Path == "/api/atlas/v1/jobs/job_1":
		f.mu.Lock()
		f.polls++
		n := f.polls
		f.mu.Unlock()
		state := "running"
		if n >= 2 {
			state = "succeeded"
		}
		_, _ = io.WriteString(w, `{"id":"job_1","tenant_id":"t","job_type":"volume.create","state":"`+state+`","requested_by":"kairon","progress_percent":50,"error":null,"result":{"bound":true},"created_at":null,"updated_at":null}`)
	case r.Method == http.MethodGet && r.URL.Path == "/api/atlas/v1/jobs/job_bad":
		_, _ = io.WriteString(w, `{"id":"job_bad","tenant_id":"t","job_type":"volume.create","state":"failed","requested_by":"kairon","progress_percent":100,"error":"pvc never bound","result":null}`)
	case r.Method == http.MethodGet && r.URL.Path == "/api/atlas/v1/volumes/vol_1":
		_, _ = io.WriteString(w, `{"id":"vol_1","cluster_id":null,"pool_id":null,"name":"web-root","kind":"block","backend_native_id":"rbd:rbd-nvme-prod/csi-vol-1","size_bytes":10737418240,"used_bytes":null,"state":"available","health":"ok","kubernetes_namespace":"default","pvc_name":"web-root","storage_class_name":"zyvor-rbd-prod"}`)
	case r.Method == http.MethodGet && r.URL.Path == "/api/atlas/v1/volumes/missing":
		w.WriteHeader(http.StatusNotFound)
		_, _ = io.WriteString(w, `{"error":{"code":"NOT_FOUND","message":"volume missing not found"}}`)
	case r.Method == http.MethodGet && r.URL.Path == "/api/atlas/v1/volumes":
		if r.URL.Query().Get("tenant") != "acme" || r.URL.Query().Get("kind") != "block" {
			f.t.Errorf("filter not sent: %s", r.URL.RawQuery)
		}
		_, _ = io.WriteString(w, `[{"id":"vol_1","name":"web-root","kind":"block","size_bytes":1,"state":"available","health":"ok"}]`)
	case r.Method == http.MethodDelete && r.URL.Path == "/api/atlas/v1/volumes/vol_native":
		_, _ = io.WriteString(w, `{"volume_id":"vol_native","deleted":true}`)
	case r.Method == http.MethodDelete && r.URL.Path == "/api/atlas/v1/snapshots/snap_1":
		if r.URL.Query().Get("force") != "true" {
			w.WriteHeader(http.StatusConflict)
			_, _ = io.WriteString(w, `{"error":{"code":"CONFLICT","message":"snapshot has clones"}}`)
			return
		}
		w.WriteHeader(http.StatusAccepted)
		_, _ = io.WriteString(w, `{"job_id":"job_2","state":"queued","resource":{"snapshot_id":"snap_1","forced":true}}`)
	case r.Method == http.MethodPost && r.URL.Path == "/api/atlas/v1/rbd-images":
		w.WriteHeader(http.StatusAccepted)
		_, _ = io.WriteString(w, `{"job_id":"job_3","state":"queued","resource":{"volume_id":"vol_rbd_web","rbd":"rbd/web"}}`)
	default:
		f.t.Errorf("unexpected %s %s", r.Method, r.URL.Path)
		http.NotFound(w, r)
	}
}

func newTest(t *testing.T) (*Client, *fakeGateway) {
	t.Helper()
	f := &fakeGateway{t: t}
	srv := httptest.NewServer(f)
	t.Cleanup(srv.Close)
	c, err := New(srv.URL+"/api/atlas/v1/", WithToken("tok"), WithUserAgent("test"))
	if err != nil {
		t.Fatal(err)
	}
	return c, f
}

func TestCreateVolumeAndWait(t *testing.T) {
	c, f := newTest(t)
	ctx := context.Background()
	a, err := c.CreateVolume(ctx, CreateVolumeRequest{
		TenantID: "acme", Name: "web-root", SizeBytes: 10 << 30, Kind: KindBlock, Policy: "database",
		Owner:      &Owner{Product: "kairon", ResourceType: "machine", ResourceID: "uid-1", Role: "root_disk"},
		Kubernetes: &KubernetesOpts{Namespace: "default", CreatePVC: true},
	})
	if err != nil {
		t.Fatal(err)
	}
	if a.JobID != "job_1" || a.Done || a.Resource.VolumeID != "vol_1" || a.Resource.PVC != "web-root" {
		t.Fatalf("accepted = %+v", a)
	}
	j, err := c.Wait(ctx, a, 5*time.Millisecond)
	if err != nil || j.State != JobSucceeded || f.polls != 2 {
		t.Fatalf("wait = %+v, %v (polls %d)", j, err, f.polls)
	}
	v, err := c.GetVolume(ctx, a.Resource.VolumeID)
	if err != nil {
		t.Fatal(err)
	}
	pool, image, ok := ParseRBD(v.NativeID())
	if !ok || pool != "rbd-nvme-prod" || image != "csi-vol-1" || *v.PVCName != "web-root" {
		t.Fatalf("volume = %+v (%s %s %v)", v, pool, image, ok)
	}
}

func TestErrorsKeepStatus(t *testing.T) {
	c, _ := newTest(t)
	ctx := context.Background()
	_, err := c.GetVolume(ctx, "missing")
	if !IsNotFound(err) || !strings.Contains(err.Error(), "NOT_FOUND") {
		t.Fatalf("not found: %v", err)
	}
	if _, err := c.DeleteSnapshot(ctx, "snap_1", false); !IsConflict(err) {
		t.Fatalf("conflict: %v", err)
	}
	a, err := c.DeleteSnapshot(ctx, "snap_1", true)
	if err != nil || a.JobID != "job_2" {
		t.Fatalf("forced delete: %+v %v", a, err)
	}
	bad, _ := New(c.baseURL.String())
	if err := bad.Health(ctx); err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("unauthenticated health: %v", err)
	}
}

func TestJobFailure(t *testing.T) {
	c, _ := newTest(t)
	_, err := c.WaitJob(context.Background(), "job_bad", time.Millisecond)
	if !IsJobFailed(err) || !strings.Contains(err.Error(), "pvc never bound") {
		t.Fatalf("job failure: %v", err)
	}
}

func TestWaitHonoursContext(t *testing.T) {
	c, _ := newTest(t)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	_, err := c.WaitJob(ctx, "job_1", time.Hour)
	if err != nil {
		// job_1 succeeds on the 2nd poll; with a 1h interval the deadline must fire first.
		if !strings.Contains(err.Error(), "deadline") {
			t.Fatalf("ctx: %v", err)
		}
		return
	}
	t.Fatal("expected deadline error")
}

func TestSynchronousWriteAndFilters(t *testing.T) {
	c, f := newTest(t)
	ctx := context.Background()
	a, err := c.DeleteVolume(ctx, "vol_native")
	if err != nil || !a.Done || a.Resource.VolumeID != "vol_native" {
		t.Fatalf("sync delete: %+v %v", a, err)
	}
	if j, err := c.Wait(ctx, a, 0); err != nil || j.State != JobSucceeded {
		t.Fatalf("wait on sync write: %+v %v", j, err)
	}
	vols, err := c.ListVolumes(ctx, VolumeFilter{Tenant: "acme", Kind: KindBlock})
	if err != nil || len(vols) != 1 || vols[0].NativeID() != "" {
		t.Fatalf("list: %+v %v", vols, err)
	}
	r, err := c.CreateRBDImage(ctx, CreateRBDImageRequest{Name: "web", SizeBytes: 1 << 30})
	if err != nil || r.Resource.RBD != "rbd/web" || r.Resource.VolumeID != "vol_rbd_web" {
		t.Fatalf("rbd: %+v %v", r, err)
	}
	if v, err := c.Version(ctx); err != nil || v.API != "v1" {
		t.Fatalf("version: %+v %v", v, err)
	}
	if !strings.HasPrefix(f.calls[0], "DELETE /api/atlas/v1/volumes/vol_native") {
		t.Fatalf("base path not normalized: %v", f.calls)
	}
}

func TestParseRBD(t *testing.T) {
	for in, want := range map[string]bool{
		"rbd:pool/img": true, "pool/img": true, "rbd:pool": false, "pvc/ns/name": false,
		"server:/export": false, "rbd:pool/img@snap": false, "": false,
	} {
		if _, _, ok := ParseRBD(in); ok != want {
			t.Errorf("ParseRBD(%q) ok=%v want %v", in, ok, want)
		}
	}
}

func TestNewRejectsBadURL(t *testing.T) {
	for _, u := range []string{"", "ftp://x", "://"} {
		if _, err := New(u); err == nil {
			t.Errorf("New(%q) accepted", u)
		}
	}
}
