import assert from 'node:assert/strict';
import { test } from 'node:test';
import { githubReleaseStore } from './github-release.mjs';
const sha='a'.repeat(40);
function fixture(draft=true) {
  const calls=[];let release={id:7,draft,prerelease:false,target_commitish:sha,assets:[]};
  const run=args=>{calls.push(args); if(args[0]==='api'){assert.ok(!args[1].includes('/releases/tags/'),'by-tag API cannot discover drafts');return args[1].includes('/commits/')?`${sha}\n`:JSON.stringify(release);} return '';};
  return {store:githubReleaseStore({run}),calls,setRelease:value=>release=value};
}
test('requires remote tag provenance before any mutation',async()=>{
  const f=fixture();await assert.rejects(f.store.begin('1.2','b'.repeat(40)),/verified source/);assert.equal(f.calls.length,1);
});
test('never uploads to a published release or temporarily removes latest on retries',async()=>{
  const f=fixture(false);await f.store.begin('1.2',sha);await assert.rejects(f.store.put('appcast.xml','/tmp/appcast.xml'),/published release/);await f.store.publish();assert.ok(f.calls.every(args=>args[0]==='api'));
});
test('draft assets have no clobber and publish excludes latest until explicit promotion',async()=>{
  const f=fixture();await f.store.begin('1.2',sha);await f.store.put('appcast.xml','/tmp/appcast.xml');await f.store.publish();await f.store.promote();
  const mutations=f.calls.filter(args=>args[0]==='release');assert.ok(!mutations.flat().includes('--clobber'));assert.ok(mutations[1].includes('--latest=false'));assert.ok(mutations[2].includes('--latest'));
});

test('uses the created draft response and stable ID instead of a stale release listing',async()=>{
  const calls=[]; const release={id:7,draft:true,prerelease:false,target_commitish:sha,assets:[]};
  const run=args=>{calls.push(args);if(args[1].includes('/commits/'))return sha;if(args.includes('--paginate'))return '';return JSON.stringify(release);};
  const store=githubReleaseStore({run});await store.begin('1.2',sha);assert.equal((await store.read('manifest.json')).status,404);
  assert.equal(calls.filter(args=>args.includes('--paginate')).length,1);assert.ok(calls.some(args=>args.includes('POST')));assert.ok(calls.some(args=>args[1].endsWith('/releases/7')));
});
