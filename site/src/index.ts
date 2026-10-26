import { WorkerEntrypoint } from "cloudflare:workers";
import { releaseResponse } from "./releases.ts";
export default class extends WorkerEntrypoint<Env> {
  async fetch(request: Request): Promise<Response> {
    if (!["GET", "HEAD"].includes(request.method)) return new Response("Method not allowed", {status:405, headers:{Allow:"GET, HEAD"}});
    const release = await releaseResponse(request);
    if (release) return release;
    const response = await this.env.ASSETS.fetch(request);
    const headers = new Headers(response.headers);
    headers.set("X-Content-Type-Options","nosniff"); headers.set("Referrer-Policy","strict-origin-when-cross-origin");
    return new Response(response.body,{status:response.status,headers});
  }
}
