{{/*
Copyright (c) 2026 ZyvorAI Labs Private Limited.
SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
*/}}

{{- define "atlas.name" -}}
{{- .Values.nameOverride | default .Chart.Name -}}
{{- end -}}

{{- define "atlas.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride -}}
{{- else -}}
{{- $name := include "atlas.name" . -}}
{{- if .Values.ceph.enabled -}}
{{- printf "%s-gateway-ceph" $name -}}
{{- else -}}
{{- printf "%s-gateway" $name -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "atlas.namespace" -}}
{{- if .Values.ceph.enabled -}}
{{- .Values.ceph.rookNamespace -}}
{{- else -}}
{{- .Values.namespace.name -}}
{{- end -}}
{{- end -}}

{{- define "atlas.labels" -}}
app: {{ include "atlas.fullname" . }}
app.kubernetes.io/name: {{ include "atlas.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "atlas.authSecretName" -}}
{{- if .Values.auth.createSecret -}}
{{- include "atlas.fullname" . }}-auth
{{- else -}}
{{- .Values.auth.existingSecret -}}
{{- end -}}
{{- end -}}

{{- define "atlas.stateBackupEndpoint" -}}
{{- .Values.stateBackup.endpoint -}}
{{- end -}}

{{/* Cluster-scoped names must be unique per release: two installs (or one next to the raw
     deploy/k8s manifest, which owns `atlas-gateway-readonly`) would otherwise collide on the
     ClusterRole/ClusterRoleBinding. */}}
{{- define "atlas.clusterRoleName" -}}
{{- printf "%s-%s-readonly" (include "atlas.fullname" .) (include "atlas.namespace" .) -}}
{{- end -}}
