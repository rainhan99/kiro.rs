import { createAdminClient } from './admin-client'
import type {
  ClientKeysResponse,
  CreateClientKeyRequest,
  CreateClientKeyResponse,
  UpdateClientKeyRequest,
  SuccessResponse,
} from '@/types/api'

const api = createAdminClient()

export async function listClientKeys(): Promise<ClientKeysResponse> {
  const { data } = await api.get<ClientKeysResponse>('/client-keys')
  return data
}

export async function createClientKey(
  req: CreateClientKeyRequest,
): Promise<CreateClientKeyResponse> {
  const { data } = await api.post<CreateClientKeyResponse>('/client-keys', req)
  return data
}

export async function deleteClientKey(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/client-keys/${id}`)
  return data
}

export async function updateClientKey(
  id: number,
  req: UpdateClientKeyRequest,
): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(`/client-keys/${id}`, req)
  return data
}

export async function setClientKeyDisabled(
  id: number,
  disabled: boolean,
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/client-keys/${id}/disabled`, { disabled })
  return data
}

export async function resetClientKeyStats(id: number): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/client-keys/${id}/reset-stats`)
  return data
}

/** 设置或清除积分使用上限。maxCredits 传 null 表示取消限制 */
export async function setClientKeyMaxCredits(
  id: number,
  maxCredits: number | null,
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/client-keys/${id}/max-credits`, { maxCredits })
  return data
}

export async function rotateClientKey(id: number): Promise<CreateClientKeyResponse> {
  const { data } = await api.post<CreateClientKeyResponse>(`/client-keys/${id}/rotate`)
  return data
}
