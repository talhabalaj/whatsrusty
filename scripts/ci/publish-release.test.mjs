import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, test } from 'node:test';
import { publishRelease, verifySignedFeed } from './publish-release.mjs';
import { releaseFiles } from './release-files.mjs';
const temporary = [];
afterEach(() => { for (const directory of temporary.splice(0)) rmSync(directory, { recursive:true, force:true }); });
function signedFeed(content, key) {
  const bytes=Buffer.from(content);
  return `${content}<!-- sparkle-signatures:\nedSignature: ${sign(null,bytes,key).toString('base64')}\nlength: ${bytes.length}\n-->\n`;
}
function fixture() {
  const output=mkdtempSync(join(tmpdir(),'whatsrusty-publisher-')); temporary.push(output);
  const keys=generateKeyPairSync('ed25519');
  const publicKey=keys.publicKey.export({type:'spki',format:'der'}).subarray(-32).toString('base64');
  const archive=Buffer.from('signed archive fixture');
  const metadata={version:'1.1',build:'8',minimumMacOS:'26.0',bytes:archive.length,sha256:createHash('sha256').update(archive).digest('hex'),signature:sign(null,archive,keys.privateKey).toString('base64'),commitSha:'b'.repeat(40),architecture:'arm64',notaryId:'0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d',builder:'github-actions run 1'};
  const {manifest,appcast}=releaseFiles(metadata), feed=signedFeed(appcast,keys.privateKey);
  for(const [name,content] of Object.entries({'manifest.json':JSON.stringify(manifest),'metadata.json':JSON.stringify(metadata),[manifest.file]:archive,[`${manifest.file}.sha256`]:`${manifest.sha256}  ${manifest.file}\n`,'appcast.xml':feed})) writeFileSync(join(output,name),content);
  const assets=new Map(), events=[], hooks={};
  let current={build:'7',sha256:'c'.repeat(64)}, published=false;
  const response=content=>content===undefined ? new Response(null,{status:404}) : new Response(content);
  const store={
    async begin(version,sha){assert.equal(version,'1.1');assert.equal(sha,metadata.commitSha);events.push('begin');},
    async read(name){return response(assets.get(name));},
    async put(name,file){events.push(`write:${name}`);if(await hooks.write?.(name,file))return; assets.set(name,readFileSync(file));},
    async publish(){events.push('publish');published=true;},
    async promote(){events.push('promote');current=manifest;},
  };
  const fetch=async url=>{
    events.push(`fetch:${new URL(url).pathname}`);
    const override=await hooks.fetch?.(url); if(override)return override;
    if(url.includes('/releases/download/')) { assert.ok(published);return response(assets.get(new URL(url).pathname.split('/').at(-1))); }
    if(url.endsWith('/downloads/manifest.json'))return current===null?new Response(null,{status:404}):Response.json(current);
    if(url.endsWith('/appcast.xml'))return response(assets.get('appcast.xml'));
    assert.fail(url);
  };
  return {output,keys,publicKey,archive,manifest,feed,appcast,assets,events,hooks,setCurrent:value=>current=value,publish:(options={})=>publishRelease(output,{publicKey,fetch,store,settleDelayMs:0,...options})};
}
test('validates Sparkle signed UTF-8 feed bytes and refuses tampering',()=>{
  const f=fixture(), feed=signedFeed('<rss>日本語</rss>\n',f.keys.privateKey);
  assert.equal(verifySignedFeed(Buffer.from(feed),f.keys.publicKey),'<rss>日本語</rss>\n');
  assert.throws(()=>verifySignedFeed(Buffer.from(feed.replace('日本語','changed')),f.keys.publicKey),/verification/);
});
test('verifies every draft and public asset before latest promotion',async()=>{
  const f=fixture();const result=await f.publish();
  assert.deepEqual(f.events.filter(e=>e.startsWith('write:')),[`write:${f.manifest.file}`,`write:${f.manifest.file}.sha256`,'write:manifest.json','write:appcast.xml']);
  const publish=f.events.indexOf('publish'), promote=f.events.indexOf('promote');
  assert.ok(publish<promote);assert.equal(f.events.slice(publish+1,promote).filter(e=>e.includes('/releases/download/')).length,4);
  assert.equal(result.download,`https://whatsrusty.apps.balaj.me/downloads/${f.manifest.file}`);
  assert.ok(existsSync(join(f.output,'publication.json')));
});
test('matching retries never rewrite immutable assets',async()=>{
  const f=fixture();await f.publish();f.events.length=0;await f.publish();assert.ok(!f.events.some(e=>e.startsWith('write:')));
});
test('orphaned or corrupt immutable assets fail before publishing',async()=>{
  for(const name of ['archive','checksum','manifest','feed']){
    const f=fixture();const key={archive:f.manifest.file,checksum:`${f.manifest.file}.sha256`,manifest:'manifest.json',feed:'appcast.xml'}[name];
    f.assets.set(key,Buffer.from('different'));await assert.rejects(f.publish(),/hash or size|Immutable/);assert.ok(!f.events.includes('publish'));
  }
});
test('older or conflicting current builds fail before GitHub mutation',async()=>{
  for(const build of ['9','8']){const f=fixture();f.setCurrent({build,sha256:'d'.repeat(64)});await assert.rejects(f.publish(),/newer release|different archive/);assert.ok(!f.events.includes('begin'));}
});
test('rechecks current build immediately before latest promotion',async()=>{
  const f=fixture();let reads=0;f.hooks.fetch=url=>{if(url.endsWith('downloads/manifest.json')&&++reads===2)return Response.json({build:'9',sha256:'c'.repeat(64)});};
  await assert.rejects(f.publish(),/newer release/);assert.ok(!f.events.includes('promote'));
});
test('failed uploads and anonymous public corruption never promote a release',async()=>{
  for(const mode of ['upload','public-archive','public-feed']){
    const f=fixture();
    f.hooks.write=()=>{if(mode==='upload')throw new Error('upload failed');};
    f.hooks.fetch=url=>{if(url.includes('/releases/download/')&&url.endsWith(mode==='public-archive'?'.zip':'appcast.xml'))return new Response('corrupt');};
    await assert.rejects(f.publish(),/upload failed|hash or size|Public release asset/);assert.ok(!f.events.includes('promote'));assert.equal(existsSync(join(f.output,'publication.json')),false);
  }
});
test('unsigned, tampered, mismatched and wrong-key feeds fail before network operations',async()=>{
  for(const mode of ['unsigned','tampered','mismatch','wrong-key']){
    const f=fixture();
    if(mode==='unsigned')writeFileSync(join(f.output,'appcast.xml'),f.appcast);
    if(mode==='tampered')writeFileSync(join(f.output,'appcast.xml'),f.feed.replace('WhatsRusty Updates','Altered Updates'));
    if(mode==='mismatch')writeFileSync(join(f.output,'appcast.xml'),signedFeed(f.appcast.replace('<sparkle:version>8</sparkle:version>','<sparkle:version>6</sparkle:version>'),f.keys.privateKey));
    if(mode==='wrong-key')writeFileSync(join(f.output,'appcast.xml'),signedFeed(f.appcast,generateKeyPairSync('ed25519').privateKey));
    await assert.rejects(f.publish(),/signed by Sparkle|verification|describe the verified/);assert.deepEqual(f.events,[]);
  }
});
test('supports the first publication and expanded enclosure serialization',async()=>{
  const f=fixture();f.setCurrent(null);writeFileSync(join(f.output,'appcast.xml'),signedFeed(f.appcast.replace(/(<enclosure[^>]+) \/>/,'$1></enclosure>'),f.keys.privateKey));assert.equal((await f.publish()).build,'8');
});
test('waits for the promoted manifest and feed to propagate',async()=>{
  const f=fixture(); let stale=2;
  f.hooks.fetch=url=>{ if(f.events.includes('promote')&&url.endsWith('/downloads/manifest.json')&&stale-->0) return Response.json({build:'7',sha256:'c'.repeat(64)}); };
  await f.publish(); assert.ok(existsSync(join(f.output,'publication.json')));
});
test('fails when the promoted manifest never propagates',async()=>{
  const f=fixture();
  f.hooks.fetch=url=>{ if(f.events.includes('promote')&&url.endsWith('/downloads/manifest.json')) return Response.json({build:'7',sha256:'c'.repeat(64)}); };
  await assert.rejects(f.publish({settleAttempts:3}),/Published manifest differs/); assert.equal(existsSync(join(f.output,'publication.json')),false);
});
