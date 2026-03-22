import { handleWorkerRequest, type WorkerEnv } from './proxy'

export default {
  async fetch(request: Request, env: WorkerEnv) {
    return handleWorkerRequest(request, env)
  },
}
