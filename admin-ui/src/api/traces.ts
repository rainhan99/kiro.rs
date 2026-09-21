import { createAdminClient } from './admin-client'
import type { FailureStatsMap, TracePage, TracePipelineEvidence, TraceQuery } from '@/types/api'

const api = createAdminClient()

export async function getTraces(query: TraceQuery): Promise<TracePage> {
  const params: Record<string, string> = {}
  if (query.status) params.status = query.status
  if (query.errorType) params.errorType = query.errorType
  if (query.credentialId != null) params.credentialId = String(query.credentialId)
  if (query.keyId != null) params.keyId = String(query.keyId)
  if (query.failedAttemptCredentialId != null)
    params.failedAttemptCredentialId = String(query.failedAttemptCredentialId)
  if (query.model) params.model = query.model
  if (query.group) params.group = query.group
  if (query.onlyFailed) params.onlyFailed = 'true'
  if (query.sessionId) params.sessionId = query.sessionId
  if (query.onlySwitched) params.onlySwitched = 'true'
  if (query.clientIp) params.clientIp = query.clientIp
  if (query.startTime != null) params.startTime = String(query.startTime)
  if (query.endTime != null) params.endTime = String(query.endTime)
  if (query.q) params.q = query.q
  if (query.limit != null) params.limit = String(query.limit)
  if (query.offset != null) params.offset = String(query.offset)
  const { data } = await api.get<TracePage>('/traces', { params })
  return data
}

export async function getFailureStats(): Promise<FailureStatsMap> {
  const { data } = await api.get<FailureStatsMap>('/traces/failure-stats')
  return data
}

export async function getTracePipelineEvidence(traceId: string): Promise<TracePipelineEvidence> {
  const { data } = await api.get<TracePipelineEvidence>(`/traces/${encodeURIComponent(traceId)}/pipeline`)
  return data
}
