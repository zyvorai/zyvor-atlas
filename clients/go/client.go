// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

// Package atlas is a Go client for the Atlas storage control plane REST API
// (base path /api/atlas/v1). It uses only the Go standard library so products
// with strict dependency policies (Kairon's controller and node) can import it.
package atlas

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

const (
	// APIPrefix is the REST base path every resource route lives under.
	APIPrefix = "/api/atlas/v1"
	// maxErrorBody bounds how much of a non-2xx body is read into an APIError.
	maxErrorBody = 64 << 10
)

// Client talks to one Atlas gateway. The zero value is not usable; use New.
type Client struct {
	baseURL   *url.URL
	token     string
	http      *http.Client
	userAgent string
}

// Option configures a Client.
type Option func(*Client)

// WithToken sets the bearer JWT sent as "Authorization: Bearer <token>".
// Required when the gateway runs with ATLAS_AUTH_REQUIRED=1.
func WithToken(token string) Option { return func(c *Client) { c.token = token } }

// WithHTTPClient replaces the default http.Client (30s timeout), for custom
// TLS roots, proxies or transports.
func WithHTTPClient(h *http.Client) Option { return func(c *Client) { c.http = h } }

// WithUserAgent sets the User-Agent header, e.g. "kairon-controller/v0.7".
func WithUserAgent(ua string) Option { return func(c *Client) { c.userAgent = ua } }

// New returns a client for the gateway at baseURL, e.g. "http://atlas:5110".
// A trailing APIPrefix in baseURL is tolerated.
func New(baseURL string, opts ...Option) (*Client, error) {
	u, err := url.Parse(strings.TrimSpace(baseURL))
	if err != nil {
		return nil, fmt.Errorf("atlas: parse base URL: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, fmt.Errorf("atlas: base URL %q must be http or https", baseURL)
	}
	u.Path = strings.TrimSuffix(strings.TrimSuffix(u.Path, "/"), APIPrefix)
	c := &Client{baseURL: u, http: &http.Client{Timeout: 30 * time.Second}, userAgent: "atlas-go"}
	for _, o := range opts {
		o(c)
	}
	return c, nil
}

// APIError is a non-2xx response. Atlas encodes errors as
// {"error":{"code":"...","message":"..."}}; Status keeps the HTTP status so
// callers can propagate it (a 409 stays a 409).
type APIError struct {
	Status  int
	Code    string
	Message string
}

func (e *APIError) Error() string {
	if e.Code == "" && e.Message == "" {
		return fmt.Sprintf("atlas: HTTP %d", e.Status)
	}
	return fmt.Sprintf("atlas: HTTP %d %s: %s", e.Status, e.Code, e.Message)
}

// IsNotFound reports whether err is an Atlas 404.
func IsNotFound(err error) bool { return statusIs(err, http.StatusNotFound) }

// IsConflict reports whether err is an Atlas 409 (name collision, quota, in-use snapshot).
func IsConflict(err error) bool { return statusIs(err, http.StatusConflict) }

// IsUnavailable reports whether err is an Atlas 503 (for example a cordoned backend).
func IsUnavailable(err error) bool { return statusIs(err, http.StatusServiceUnavailable) }

func statusIs(err error, status int) bool {
	var ae *APIError
	return errors.As(err, &ae) && ae.Status == status
}

func (c *Client) endpoint(path string, q url.Values) string {
	u := *c.baseURL
	u.Path = u.Path + path
	if len(q) > 0 {
		u.RawQuery = q.Encode()
	}
	return u.String()
}

// do sends a request and decodes a 2xx JSON body into out (if non-nil).
// It returns the HTTP status so callers can tell 200 (done) from 202 (job).
func (c *Client) do(ctx context.Context, method, path string, q url.Values, body, out any) (int, error) {
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return 0, fmt.Errorf("atlas: encode request: %w", err)
		}
		rd = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.endpoint(path, q), rd)
	if err != nil {
		return 0, fmt.Errorf("atlas: build request: %w", err)
	}
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	if c.userAgent != "" {
		req.Header.Set("User-Agent", c.userAgent)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return 0, fmt.Errorf("atlas: %s %s: %w", method, path, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return resp.StatusCode, decodeError(resp)
	}
	if out == nil {
		_, _ = io.Copy(io.Discard, resp.Body)
		return resp.StatusCode, nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return resp.StatusCode, fmt.Errorf("atlas: decode %s %s: %w", method, path, err)
	}
	return resp.StatusCode, nil
}

func decodeError(resp *http.Response) error {
	b, _ := io.ReadAll(io.LimitReader(resp.Body, maxErrorBody))
	ae := &APIError{Status: resp.StatusCode}
	var env struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(b, &env) == nil && (env.Error.Code != "" || env.Error.Message != "") {
		ae.Code, ae.Message = env.Error.Code, env.Error.Message
	} else {
		ae.Message = strings.TrimSpace(string(b))
	}
	return ae
}

// Health calls GET /health.
func (c *Client) Health(ctx context.Context) error {
	_, err := c.do(ctx, http.MethodGet, "/health", nil, nil, nil)
	return err
}

// Version calls GET /version.
func (c *Client) Version(ctx context.Context) (Version, error) {
	var v Version
	_, err := c.do(ctx, http.MethodGet, "/version", nil, nil, &v)
	return v, err
}
