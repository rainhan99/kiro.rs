import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { getRequestPipeline, saveRequestPipeline } from '@/api/request-pipeline'

const queryKey = ['request-pipeline']
export function useRequestPipeline() {
  return useQuery({ queryKey, queryFn: getRequestPipeline, refetchOnWindowFocus: false, retry: false })
}

export function useSaveRequestPipeline() {
  const client = useQueryClient()
  return useMutation({
    mutationFn: saveRequestPipeline,
    onSuccess: (snapshot) => client.setQueryData(queryKey, snapshot),
  })
}
