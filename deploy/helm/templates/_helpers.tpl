{{- define "blue.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- define "blue.fullname" -}}
{{- if .Values.fullnameOverride }}{{ .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}{{- else }}{{ printf "%s-%s" .Release.Name (include "blue.name" .) | trunc 63 | trimSuffix "-" }}{{- end }}
{{- end }}
{{- define "blue.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | quote }}
app.kubernetes.io/name: {{ include "blue.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}
{{- define "blue.selectorLabels" -}}
app.kubernetes.io/name: {{ include "blue.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
{{- define "blue.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}{{ default (include "blue.fullname" .) .Values.serviceAccount.name }}{{- else }}{{ default "default" .Values.serviceAccount.name }}{{- end }}
{{- end }}
{{- define "blue.image" -}}
{{- if .Values.image.digest }}{{ printf "%s@%s" .Values.image.repository .Values.image.digest }}{{- else }}{{ printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}{{- end }}
{{- end }}
{{- define "blue.provisionerExecutableImage" -}}
{{- printf "%s@%s" .Values.blue.provisionerExecutable.image.repository .Values.blue.provisionerExecutable.image.digest }}
{{- end }}
{{- define "blue.validate" -}}
{{- if and .Values.blue.production (not .Values.blue.existingSecret) -}}
{{- fail "blue.existingSecret is required when blue.production=true" -}}
{{- end -}}
{{- if hasKey .Values.blue.env "BLUE_ENVIRONMENT" -}}
{{- fail "blue.env.BLUE_ENVIRONMENT is reserved; Helm workloads always use production safety validation" -}}
{{- end -}}
{{- if hasKey .Values.blue.env "BLUE_GATEWAY_ENABLED" -}}
{{- fail "blue.env.BLUE_GATEWAY_ENABLED is reserved; use blue.enableInferenceProxy" -}}
{{- end -}}
{{- if and .Values.blue.production (not .Values.image.digest) -}}
{{- fail "image.digest is required when blue.production=true; mutable tags are evaluation-only" -}}
{{- end -}}
{{- if ne (int .Values.components.worker.replicas) 1 -}}
{{- fail "components.worker.replicas must be 1; the background worker is a singleton" -}}
{{- end -}}
{{- if and .Values.blue.production (not .Values.migrations.enabled) -}}
{{- fail "migrations.enabled must be true when blue.production=true" -}}
{{- end -}}
{{- if and .Values.blue.production (not .Values.networkPolicy.enabled) -}}
{{- fail "networkPolicy.enabled must be true when blue.production=true" -}}
{{- end -}}
{{- if and .Values.blue.enableInferenceProxy (not .Values.blue.gatewayType) -}}
{{- fail "blue.gatewayType is required when blue.enableInferenceProxy=true" -}}
{{- end -}}
{{- if and .Values.blue.enableInferenceProxy (eq .Values.blue.internalTransport.mode "mtls") (or (not .Values.blue.internalTransport.serverSecret) (not .Values.blue.internalTransport.clientSecret)) -}}
{{- fail "blue.internalTransport.serverSecret and clientSecret are required for gateway mTLS" -}}
{{- end -}}
{{- end }}
