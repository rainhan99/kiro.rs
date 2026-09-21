import { createAdminClient } from './admin-client'
import type { RequestPipelineSnapshot, SaveRequestPipeline } from '@/types/request-pipeline'

const api = createAdminClient()

export async function getRequestPipeline(): Promise<RequestPipelineSnapshot> {
  return (await api.get<RequestPipelineSnapshot>('/request-pipeline')).data
}

export async function getContextCalibration(): Promise<{ observations: unknown }> {
  return (await api.get<{ observations: unknown }>('/context-calibration')).data
}

export async function saveRequestPipeline(payload: SaveRequestPipeline): Promise<RequestPipelineSnapshot> {
  return (await api.put<RequestPipelineSnapshot>('/request-pipeline', payload)).data
}
