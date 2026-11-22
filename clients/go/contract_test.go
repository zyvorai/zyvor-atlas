// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

package atlas

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"testing"
	"time"
)

// TestContract drives a real atlas-gateway (fake drivers are enough) so the
// Go types cannot drift from the Rust routes unnoticed. CI starts the gateway
// and sets ATLAS_CONTRACT_URL; locally:
//
//	cargo run -p atlas-gateway &   # fake drivers, 127.0.0.1:5110
//	ATLAS_CONTRACT_URL=http://127.0.0.1:5110 go test -run Contract ./...
func TestContract(t *testing.T) {
	base := os.Getenv("ATLAS_CONTRACT_URL")
	if base == "" {
		t.Skip("ATLAS_CONTRACT_URL not set")
	}
	c, err := New(base, WithToken(os.Getenv("ATLAS_CONTRACT_TOKEN")), WithUserAgent("atlas-go-contract"))
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()

	if err := c.Health(ctx); err != nil {
		t.Fatalf("health: %v", err)
	}
	if v, err := c.Version(ctx); err != nil || v.API != "v1" {
		t.Fatalf("version: %+v %v", v, err)
	}

	name := fmt.Sprintf("go-contract-%d", time.Now().UnixNano())
	a, err := c.CreateRBDImage(ctx, CreateRBDImageRequest{Name: name, SizeBytes: 1 << 30})
	if err != nil {
		t.Fatalf("create rbd image: %v", err)
	}
	if a.Resource.VolumeID == "" || a.Resource.RBD == "" {
		t.Fatalf("create rbd image resource: %+v", a)
	}
	if _, err := c.Wait(ctx, a, 200*time.Millisecond); err != nil {
		t.Fatalf("wait create: %v", err)
	}
	v, err := c.GetVolume(ctx, a.Resource.VolumeID)
	if err != nil {
		t.Fatalf("get volume: %v", err)
	}
	pool, image, ok := ParseRBD(v.NativeID())
	if !ok || image != name || pool+"/"+image != a.Resource.RBD || v.SizeBytes != 1<<30 || v.Kind != KindBlock {
		t.Fatalf("volume %+v (native %q)", v, v.NativeID())
	}
	vols, err := c.ListVolumes(ctx, VolumeFilter{Kind: KindBlock})
	if err != nil || !containsVolume(vols, v.ID) {
		t.Fatalf("list volumes: %v (found=%v)", err, containsVolume(vols, v.ID))
	}

	del, err := c.DeleteRBDImage(ctx, pool, image)
	if err != nil {
		t.Fatalf("delete rbd image: %v", err)
	}
	if _, err := c.Wait(ctx, del, 200*time.Millisecond); err != nil {
		t.Fatalf("wait delete: %v", err)
	}

	if _, err := c.GetVolume(ctx, "vol_does_not_exist"); !IsNotFound(err) {
		t.Fatalf("missing volume: %v", err)
	}
	if _, err := c.GetJob(ctx, "job_does_not_exist"); !IsNotFound(err) {
		t.Fatalf("missing job: %v", err)
	}
	_, err = c.CreateVolume(ctx, CreateVolumeRequest{TenantID: "global", Name: "Not_A_Valid_Name", SizeBytes: 1 << 30})
	if !statusIs(err, http.StatusBadRequest) {
		t.Fatalf("invalid name must be 400 VALIDATION_ERROR: %v", err)
	}
}

func containsVolume(vols []Volume, id string) bool {
	for _, v := range vols {
		if v.ID == id {
			return true
		}
	}
	return false
}
