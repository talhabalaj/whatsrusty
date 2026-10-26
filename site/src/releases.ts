const origin = 'https://github.com/talhabalaj/whatsrusty/releases';
const archive = /^WhatsRusty-(\d+(?:\.\d+){0,2})-\d+(?:\.\d+){0,2}\.zip(?:\.sha256)?$/;
const unavailable = () => new Response('The first release is being prepared.', {status:404, headers:{'Cache-Control':'no-store'}});

export async function releaseResponse(request: Request, fetchAsset: typeof fetch = fetch): Promise<Response | null> {
  const path = new URL(request.url).pathname;
  const latest = (name: string) => fetchAsset(`${origin}/latest/download/${name}`, {headers:{'Cache-Control':'no-cache'}, cache:'no-store'});
  if (path === '/downloads/latest.zip') {
    const response = await latest('manifest.json');
    if (response.status === 404) return unavailable();
    if (!response.ok) return new Response('Release unavailable', {status:502});
    const manifest = await response.json() as {file?: string; notarized?: boolean};
    if (typeof manifest.file !== 'string' || !archive.test(manifest.file) || manifest.file.endsWith('.sha256') || manifest.notarized !== true) return new Response('Release unavailable', {status:502});
    return new Response(null, {status:302, headers:{Location:`/downloads/${manifest.file}`, 'Cache-Control':'no-store'}});
  }
  const name = path === '/appcast.xml' ? 'appcast.xml' : path === '/downloads/manifest.json' ? 'manifest.json' : null;
  if (name) {
    const response = await latest(name);
    if (response.status === 404) return unavailable();
    if (!response.ok) return new Response('Release unavailable', {status:502});
    return new Response(request.method === 'HEAD' ? null : response.body, {headers:{
      'Content-Type':name.endsWith('.xml') ? 'application/rss+xml; charset=utf-8' : 'application/json',
      'Cache-Control':'no-store', 'X-Content-Type-Options':'nosniff',
    }});
  }
  const file = path.startsWith('/downloads/') ? path.slice(11) : '';
  const match = archive.exec(file);
  if (match) return new Response(null, {status:302, headers:{Location:`${origin}/download/v${match[1]}/${file}`, 'Cache-Control':'public, max-age=31536000, immutable'}});
  return null;
}
