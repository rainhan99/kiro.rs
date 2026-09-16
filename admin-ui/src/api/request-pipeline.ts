import axios from 'axios'
import { storage } from '@/lib/storage'
import type { RequestPipelineSnapshot, SaveRequestPipeline } from '@/types/request-pipeline'

const api = axios.create({ baseURL: '/api/admin', timeout: 15000, headers: { 'Content-Type': 'application/json' } })
api.interceptors.request.use((config) => {
  const key = storage.getApiKey()
  if (key) config.headers['x-api-key'] = key
  return config
})

export async function getRequestPipeline(): Promise<RequestPipelineSnapshot> {
  return (await api.get<RequestPipelineSnapshot>('/request-pipeline')).data
}

export async function saveRequestPipeline(payload: SaveRequestPipeline): Promise<RequestPipelineSnapshot> {
  return (await api.put<RequestPipelineSnapshot>('/request-pipeline', payload)).data
}
