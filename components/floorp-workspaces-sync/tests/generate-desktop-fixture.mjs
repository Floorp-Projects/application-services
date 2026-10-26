// SPDX-License-Identifier: MPL-2.0
// Usage: node --experimental-strip-types generate-desktop-fixture.mjs /path/to/pinned/desktop/common
// Desktop source revision: a218c5b9751dbfb6882721a51f916f15c5c5e99e.
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import { writeFileSync } from 'node:fs';
const {parseWorkspaceSyncRecord}=await import(pathToFileURL(resolve(process.argv[2], 'workspace-sync-record.ts')));
const W='AAAAAAAA-0000-0000-0000-000000000001';
const T='bbbbbbbb-0000-0000-0000-000000000002';
const D='cccccccc-0000-0000-0000-000000000003';
const metadata=()=>({version:1,kind:'workspace',workspaceId:W,name:'Desktop',icon:null,metadata:{modifiedAt:0,modifiedBy:D}});
const tab=()=>({version:1,kind:'tab',workspaceId:W,tab:{id:T,url:'https://example.org/',title:'Page',pinned:true,position:0,deleted:false,modifiedAt:5,modifiedBy:D}});
const cases=[];
function fixture(name,entity) {
  let normalized=null;
  try {normalized=JSON.stringify(parseWorkspaceSyncRecord(entity));} catch {}
  cases.push({name,id:entity.kind==='workspace'?W:T,payload:JSON.stringify({id:entity.kind==='workspace'?W:T,entity}),accepted:normalized!==null,normalized,navigationUrl:normalized!==null && entity.kind==='tab'?new URL(entity.tab.url).href:null});
}
fixture('metadata-clock-zero-and-uppercase-id',metadata());
fixture('tab',tab());
fixture('metadata-json-escapes',{...metadata(),name:'\n\t"\\\u2028😀'});
fixture('metadata-256-utf16',{...metadata(),name:'😀'.repeat(128)});
fixture('metadata-258-utf16',{...metadata(),name:'😀'.repeat(129)});
fixture('metadata-unpaired-surrogate',{...metadata(),name:'\ud800'});
fixture('unknown-fields-are-normalized-away',{...metadata(),future:{unknown:'preserved-only-on-server'}});
fixture('unknown-icon',{...metadata(),icon:'future-icon-name'});
fixture('missing-icon',((e)=>{delete e.icon;return e;})(metadata()));
fixture('future-version',{...metadata(),version:2});
fixture('fractional-clock',{...metadata(),metadata:{modifiedAt:0.5,modifiedBy:D}});
fixture('unsafe-clock',{...metadata(),metadata:{modifiedAt:9007199254740992,modifiedBy:D}});
fixture('maximum-safe-clock',{...metadata(),metadata:{modifiedAt:9007199254740991,modifiedBy:D}});
fixture('same-workspace-tab-id',{...tab(),tab:{...tab().tab,id:W}});
fixture('wire-tab-deletion',{...tab(),tab:{...tab().tab,deleted:true}});
fixture('title-1024-utf16',{...tab(),tab:{...tab().tab,title:'😀'.repeat(512)}});
fixture('title-1026-utf16',{...tab(),tab:{...tab().tab,title:'😀'.repeat(513)}});
for (const [name,url] of [
 ['ordinary-http','http://example.org/'],['surrounding-whitespace',' HTTPS://example.org/a '],
 ['unicode-host','https://例え.テスト/道'],['empty-credentials','https://@example.org/'],['empty-credentials-both','https://:@example.org/'],
 ['nonempty-credentials','https://user:pass@example.org/'],['private-file','file:///tmp/a'],
 ['internal','about:blank'],['invalid-port','https://example.org:bad/'],
 ['no-slash','http:example.org/path'],['embedded-tab','https://exa\tmple.org/a'],['single-slash','http:/example.org/path'],['backslash','https:\\example.org\\a'],
 ['url-utf16-limit','https://example.org/'+ 'a'.repeat(8192-20)],
 ['url-over-limit','https://example.org/'+ 'a'.repeat(8193)],
 ['entity-over-16k','https://example.org/'+ '中'.repeat(8000)],
]) fixture(name,{...tab(),tab:{...tab().tab,url}});
const output={desktopRevision:'a218c5b9751dbfb6882721a51f916f15c5c5e99e',runtime:process.version,cases};
writeFileSync(new URL('./fixtures/desktop-v1.json',import.meta.url),JSON.stringify(output,null,2)+'\n');
