{{/*
Copyright (c) 2026 ZyvorAI Labs Private Limited.
SPDX-License-Identifier: Apache-2.0
*/}}

{{- define "atlas-native.fullname" -}}
{{- if contains "atlas-native" .Release.Name -}}
{{- .Release.Name | trunc 50 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-atlas-native" .Release.Name | trunc 50 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "atlas-native.selectorLabels" -}}
app.kubernetes.io/name: atlas-native
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "atlas-native.labels" -}}
{{ include "atlas-native.selectorLabels" . }}
app.kubernetes.io/part-of: atlas
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/* Pod i's stable DNS name through the headless Service. */}}
{{- define "atlas-native.podHost" -}}
{{- $root := index . 0 -}}
{{- $i := index . 1 -}}
{{- $full := include "atlas-native.fullname" $root -}}
{{- printf "%s-%d.%s.%s.svc.%s" $full $i $full $root.Release.Namespace $root.Values.clusterDomain -}}
{{- end -}}

{{- define "atlas-native.tokenSecret" -}}
{{- .Values.apiToken.existingSecret | default (printf "%s-api" (include "atlas-native.fullname" .)) -}}
{{- end -}}

{{- define "atlas-native.tlsSecret" -}}
{{- if .Values.tls.certManager.enabled -}}
{{- printf "%s-tls" (include "atlas-native.fullname" .) -}}
{{- else -}}
{{- required "tls.existingSecret is required when tls.enabled (or enable tls.certManager)" .Values.tls.existingSecret -}}
{{- end -}}
{{- end -}}

{{- define "atlas-native.probeScheme" -}}
{{- if .Values.httpTls.enabled }}HTTPS{{ else }}HTTP{{ end -}}
{{- end -}}

{{/* The node config shared by every pod; ${POD_NAME} is expanded by the node at startup. */}}
{{- define "atlas-native.config" -}}
{{- $full := include "atlas-native.fullname" . -}}
{{- $n := int .Values.replicas -}}
{{- if lt $n 1 }}{{ fail "replicas must be at least 1" }}{{ end -}}
{{- if gt (int .Values.node.replicationFactor) $n }}{{ fail "node.replicationFactor cannot exceed replicas" }}{{ end -}}
{{- $peers := dict -}}
{{- $dataNodes := list -}}
{{- $free := .Values.node.freeBytes | int64 -}}
{{- range $i := until $n -}}
{{- $id := printf "%s-%d" $full $i -}}
{{- $host := include "atlas-native.podHost" (list $ $i) -}}
{{- $_ := set $peers $id (printf "%s:7482" $host) -}}
{{- $spec := dict "id" $id "addr" (printf "%s:7481" $host) -}}
{{- if gt $free 0 }}{{ $_ := set $spec "free_bytes" $free }}{{ end -}}
{{- $dataNodes = append $dataNodes $spec -}}
{{- end -}}
{{- $bootstrap := list -}}
{{- if .Values.membership.bootstrapReplicas -}}
{{- range $i := until (int .Values.membership.bootstrapReplicas) -}}
{{- $bootstrap = append $bootstrap (printf "%s-%d" $full $i) -}}
{{- end -}}
{{- else -}}
{{- $existing := lookup "v1" "ConfigMap" .Release.Namespace (printf "%s-config" $full) -}}
{{- if and $existing $existing.data (index $existing.data "node.json") -}}
{{- $old := index $existing.data "node.json" | fromJson -}}
{{- $bootstrap = (dig "metadata" "bootstrap" list $old) -}}
{{- end -}}
{{- if not $bootstrap -}}
{{- range $i := until $n -}}
{{- $bootstrap = append $bootstrap (printf "%s-%d" $full $i) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $cfg := dict
  "node_id" "${POD_NAME}"
  "data_dir" "/var/lib/atlas-native/state"
  "http_listen" "0.0.0.0:7480"
  "api_token_file" "/etc/atlas-native/api/token"
  "max_request_bytes" (.Values.node.maxRequestBytes | int64)
  "data_node" (dict "listen" "0.0.0.0:7481")
  "metadata" (dict
    "listen" "0.0.0.0:7482"
    "peers" $peers
    "bootstrap" $bootstrap
    "data_nodes" $dataNodes
    "replicas" (.Values.node.replicationFactor | int)
    "extent_bytes" (.Values.node.extentBytes | int64)
    "tick_ms" (.Values.node.tickMs | int)
    "proposal_timeout_ms" (.Values.node.proposalTimeoutMs | int)
    "erasure_min_bytes" (.Values.node.erasureMinBytes | int64)
    "repair_interval_secs" (.Values.node.repairIntervalSecs | int)
    "rebuild_delay_secs" (.Values.node.rebuildDelaySecs | int)
    "rebuild_bytes_per_sec" (.Values.node.rebuildBytesPerSec | int64)
    "scrub_bytes_per_sec" (.Values.node.scrubBytesPerSec | int64)
    "gc_interval_secs" (.Values.node.gcIntervalSecs | int)
    "groups" (.Values.node.metadataGroups | int)
    "cache_inodes" (.Values.node.cacheInodes | int64)
    "store_cache_bytes" (.Values.node.storeCacheBytes | int64)) -}}
{{- if .Values.tiering.enabled -}}
{{- $t := .Values.tiering -}}
{{- $byo := and $t.endpoint $t.bucket $t.existingSecret -}}
{{- if not (or $byo $t.objectBucketClaim.storageClassName) }}{{ fail "tiering needs tiering.objectBucketClaim.storageClassName, or tiering.endpoint, bucket and existingSecret" }}{{ end -}}
{{- $s3 := dict
  "endpoint" (ternary $t.endpoint "http://${BUCKET_HOST}:${BUCKET_PORT}" (not (empty $byo)))
  "bucket" (ternary $t.bucket "${BUCKET_NAME}" (not (empty $byo)))
  "region" $t.region
  "access_key_file" "/etc/atlas-native/s3/AWS_ACCESS_KEY_ID"
  "secret_key_file" "/etc/atlas-native/s3/AWS_SECRET_ACCESS_KEY" -}}
{{- $_ := set $cfg.metadata "tiering" (dict
  "store" (dict "s3" $s3)
  "prefix" $t.prefix
  "cold_after_secs" ($t.coldAfterSecs | int64)
  "interval_secs" ($t.intervalSecs | int64)
  "bytes_per_sec" ($t.bytesPerSec | int64)
  "min_extent_bytes" ($t.minExtentBytes | int64)) -}}
{{- end -}}
{{- if .Values.node.compression -}}
{{- $_ := set $cfg.metadata "compression" true -}}
{{- end -}}
{{- with .Values.node.erasure -}}
{{- $km := splitList "+" . -}}
{{- if ne (len $km) 2 }}{{ fail "node.erasure must look like \"4+2\"" }}{{ end -}}
{{- if gt (add (atoi (index $km 0)) (atoi (index $km 1))) $n }}{{ fail "node.erasure needs a pod per shard: data + parity cannot exceed replicas" }}{{ end -}}
{{- $_ := set $cfg.metadata "erasure" . -}}
{{- end -}}
{{- if .Values.tls.enabled -}}
{{- $_ := set $cfg "tls" (dict "ca" "/etc/atlas-native/tls/ca.crt" "cert" "/etc/atlas-native/tls/tls.crt" "key" "/etc/atlas-native/tls/tls.key") -}}
{{- end -}}
{{- if .Values.httpTls.enabled -}}
{{- $h := dict "cert" "/etc/atlas-native/http-tls/tls.crt" "key" "/etc/atlas-native/http-tls/tls.key" -}}
{{- if .Values.httpTls.requireClientCert }}{{ $_ := set $h "client_ca" "/etc/atlas-native/http-tls/ca.crt" }}{{ end -}}
{{- $_ := set $cfg "http_tls" $h -}}
{{- end -}}
{{- toPrettyJson $cfg -}}
{{- end -}}

{{/* The Secret holding the tiering bucket's credentials. */}}
{{- define "atlas-native.tierSecret" -}}
{{- $t := .Values.tiering -}}
{{- if and $t.endpoint $t.bucket $t.existingSecret -}}
{{- $t.existingSecret -}}
{{- else -}}
{{- printf "%s-tier" (include "atlas-native.fullname" .) -}}
{{- end -}}
{{- end -}}
