import {test} from 'node:test';
import assert from 'node:assert/strict';
import {generateKeyPairSync,sign} from 'node:crypto';
import {mkdtempSync,writeFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {createServer} from 'node:net';

test('agent verifies connector backend JWT, including health, without bearer fallback',async()=>{
  const directory=mkdtempSync(join(tmpdir(),'ffmpeg-hng-'));let child;
  try {
    const server=createServer();server.listen(0,'127.0.0.1');await once(server,'listening');const port=server.address().port;await new Promise(r=>server.close(r));
    const pair=generateKeyPairSync('ec',{namedCurve:'prime256v1'});
    const jwks=join(directory,'jwks.json');writeFileSync(jwks,JSON.stringify({keys:[{...pair.publicKey.export({format:'jwk'}),kid:'one',alg:'ES256'}]}));
    const tokenFile=join(directory,'legacy');writeFileSync(tokenFile,'a'.repeat(64));
    const bearer=(aud='ffmpeg-spark-1',expired=false,lifetime)=>{const t=Math.floor(Date.now()/1000);const iat=lifetime===undefined?t-10:t;const exp=lifetime===undefined?(expired?t-1:t+60):t+lifetime;const data=[{alg:'ES256',typ:'hng-backend+jwt',kid:'one'},{sub:'service#smss',name:'SMSS',aud,iat,exp}].map(v=>Buffer.from(JSON.stringify(v)).toString('base64url')).join('.');return 'Bearer '+data+'.'+sign('sha256',Buffer.from(data),{key:pair.privateKey,dsaEncoding:'ieee-p1363'}).toString('base64url');};
    child=spawn('target/debug/ffmpeg-agent',[],{env:{...process.env,FFMPEG_AGENT_LISTEN:`127.0.0.1:${port}`,FFMPEG_AGENT_WORK_DIR:join(directory,'work'),FFMPEG_AGENT_TOKEN_FILE:tokenFile,HNG_BACKEND_JWKS:jwks,HNG_SERVICE_ID:'ffmpeg-spark-1'},stdio:['ignore','ignore','pipe']});
    let logs='';child.stderr.on('data',b=>logs+=b);
    const request=(authorization,path='/healthz',init={})=>fetch(`http://127.0.0.1:${port}${path}`,{...init,headers:{'content-type':'application/json','x-user-id':'service#smss',...(authorization?{authorization}:{})}});
    let ready=false;for(let i=0;i<100;i++){if(child.exitCode!==null)throw new Error(logs);try{if((await request(bearer())).status===200){ready=true;break;}}catch{}await new Promise(r=>setTimeout(r,25));}assert(ready,logs);
    assert.equal((await request()).status,401);
    assert.equal((await request('Bearer '+'a'.repeat(64))).status,401);
    assert.equal((await request(bearer('smss'))).status,401);
    assert.equal((await request(bearer('ffmpeg-spark-1',true))).status,401);
    // Backend JWT lifetime bound (exp - iat <= 300 s, HNG BACKEND_MAX_LIFETIME_SECS).
    assert.equal((await request(bearer('ffmpeg-spark-1',false,300))).status,200);
    assert.equal((await request(bearer('ffmpeg-spark-1',false,301))).status,401);
    assert.equal((await request(undefined,'/v1/jobs/id',{method:'PUT',body:'{}'})).status,401);
    assert.equal((await request(bearer(),'/v1/jobs/id',{method:'PUT',body:'{}'})).status,422);
    // Atomic replacement with a new kid triggers reload in the Rust verifier.
    writeFileSync(jwks,JSON.stringify({keys:[]}));
    await new Promise(r=>setTimeout(r,5100));
    assert.equal((await request(bearer())).status,401);
  } finally {if(child&&child.exitCode===null){child.kill('SIGTERM');await once(child,'exit');}rmSync(directory,{recursive:true,force:true});}
});
