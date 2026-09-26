#!/usr/bin/env python3
"""Isolated real inferd/Apple worker test. Never edits or restarts the installed service."""
import argparse,base64,hashlib,json,os,socket,struct,subprocess,tempfile,time,urllib.request,urllib.error,zlib
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
CORE='infer-runtime.consumer-core@20260813.1'
CAP='infer.vision.apple-native@20260926.2'
def png_fixture(path):
    # Deliberately legible bitmap text; no external imaging dependency.
    glyphs={'I':['11111','00100','00100','00100','00100','00100','11111'],'N':['10001','11001','11001','10101','10011','10011','10001'],'F':['11111','10000','10000','11110','10000','10000','10000'],'E':['11111','10000','10000','11110','10000','10000','11111'],'R':['11110','10001','10001','11110','10100','10010','10001'],'2':['11111','00001','00001','11111','10000','10000','11111'],'7':['11111','00001','00010','00100','01000','01000','01000'],' ':['00000']*7}
    width,height,scale=800,180,12
    pixels=bytearray([255]*(width*height*3))
    for i,letter in enumerate('INFER 27'):
        for y,row in enumerate(glyphs[letter]):
            for x,bit in enumerate(row):
                if bit=='1':
                    for yy in range(40+y*scale,40+(y+1)*scale):
                        for xx in range(40+(i*6+x)*scale,40+(i*6+x+1)*scale):
                            offset=(yy*width+xx)*3;pixels[offset:offset+3]=b'\x00'*3
    def chunk(tag,data):return struct.pack('!I',len(data))+tag+data+struct.pack('!I',zlib.crc32(tag+data))
    rows=b''.join(b'\0'+pixels[y*width*3:(y+1)*width*3] for y in range(height))
    path.write_bytes(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('!IIBBBBB',width,height,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(rows))+chunk(b'IEND',b''))
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--photo',type=Path);parser.add_argument('--raw',type=Path,action='append',default=[]);parser.add_argument('--unsupported-raw',type=Path);parser.add_argument('--sdk',type=Path);parser.add_argument('--report',type=Path,required=True);args=parser.parse_args()
    binary=ROOT/'target/debug/inferd';worker=ROOT/'target/apple-image-worker'
    report={'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'worker_sha256':hashlib.sha256(worker.read_bytes()).hexdigest(),'checks':[]}
    with tempfile.TemporaryDirectory(prefix='infer-apple-smoke-') as tmp:
        temp=Path(tmp);fixture=temp/'ocr.png';png_fixture(fixture)
        with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
        config=(ROOT/'config/apple-image.example.toml').read_text().replace('127.0.0.1:8788',f'127.0.0.1:{port}').replace('/absolute/path/to/infer-runtime/target/apple-image-worker',str(worker)).replace('.infer-runtime/apple-example',str(temp))
        config_path=temp/'config.toml';config_path.write_text(config)
        token=os.urandom(32).hex();env=dict(os.environ,INFER_APPLE_IMAGE_TOKEN=token,INFRA_PROTOCOL_RUNTIME_DIR=str(temp/'discovery'))
        with (temp/'daemon.log').open('w+') as log:
            process=subprocess.Popen([str(binary),'--config',str(config_path)],env=env,stdout=log,stderr=log)
            try:
                base=f'http://127.0.0.1:{port}'
                for _ in range(120):
                    if process.poll() is not None:log.seek(0);raise AssertionError(log.read())
                    try:urllib.request.urlopen(base+'/health',timeout=1);break
                    except OSError:time.sleep(.1)
                else:raise AssertionError('daemon did not start')
                def call(options,path=fixture,metadata=None,auth=True,cap=CAP):
                    parameters={'model':'apple.'+options['operation'],'source_revision':'smoke-r1','options':options,'metadata':metadata or {}}
                    boundary='infer-apple-smoke'
                    body=(f'--{boundary}\r\nContent-Disposition: form-data; name="request"\r\nContent-Type: application/json\r\n\r\n'+json.dumps(parameters)+f'\r\n--{boundary}\r\nContent-Disposition: form-data; name="image"; filename="input"\r\nContent-Type: application/octet-stream\r\n\r\n').encode()+path.read_bytes()+f'\r\n--{boundary}--\r\n'.encode()
                    headers={'Content-Type':'multipart/form-data; boundary='+boundary,'Infer-Consumer-Contract':CORE,'Infer-Capability-Contract':cap}
                    if auth:headers['Authorization']='Bearer '+token
                    request=urllib.request.Request(base+'/infer/v1/vision/apple-images',data=body,headers=headers)
                    started=time.monotonic()
                    try:
                        with urllib.request.urlopen(request,timeout=125) as response:status=response.status;value=json.load(response)
                    except urllib.error.HTTPError as error:status=error.code;value=json.load(error)
                    return status,value,int((time.monotonic()-started)*1000)
                for label,kwargs,expected in [('authentication',{'auth':False},401),('capability',{'cap':'wrong@1'},426),('retired_capability',{'cap':'infer.vision.apple-native@20260926.1'},426),('privacy',{'metadata':{'infer.placement':'anywhere'}},400)]:
                    status,value,elapsed=call({'operation':'ocr'},**kwargs)
                    assert status==expected,(label,status,value)
                    report['checks'].append({'check':label,'status':status})
                removed={'operation':'describe','prompt':'Describe this image.'}
                status,value,_=call(removed)
                assert status==400 and value['error']['code']=='invalid_request_error',(status,value)
                assert value['error']['message']=='invalid Apple image parameters',value
                report['checks'].append({'check':'removed_description_rejected_before_admission','status':status})
                worker_result=subprocess.run([str(worker)],input=json.dumps(removed),capture_output=True,text=True,check=True)
                envelope=json.loads(worker_result.stdout)
                assert envelope['protocol']=='infer.apple-image-worker@20260926.2' and envelope['ok'] is False and envelope['error']=='invalid_request',envelope
                report['checks'].append({'check':'removed_description_rejected_by_worker','error':envelope['error']})
                for operation in ['ocr','aesthetics','segment']+['raw_render']*len(args.raw):
                    options={'operation':operation};photo=fixture
                    if operation=='segment':options['points']=[{'x':.3,'y':.5,'include':True}];photo=args.photo or fixture
                    if operation=='raw_render':options.update(exposure=0,noise_reduction=1);photo=args.raw.pop(0)
                    status,value,elapsed=call(options,photo)
                    receipt={'operation':operation,'http_status':status,'round_trip_ms':elapsed}
                    if status==200:
                        result=value['result'];receipt['provenance']=value['provenance']
                        assert value['source_revision']=='smoke-r1' and result['operation']==operation
                        if 'raster' in result:
                            raster=result['raster'];payload=base64.b64decode(raster['data_base64'],validate=True)
                            assert hashlib.sha256(payload).hexdigest()==raster['sha256'];receipt['raster']={k:v for k,v in raster.items() if k!='data_base64'}
                        if operation=='ocr':
                            text=' '.join(line['text'] for line in result['lines']);assert 'INFER' in text.upper(),text
                            receipt['recognized_text']=text
                        if operation=='aesthetics':assert -1<=result['overall_score']<=1;receipt['overall_score']=result['overall_score']
                    else:receipt['error']=value
                    assert status==200,(operation,status,value)
                    if operation=='raw_render':
                        assert result['decoder_version'] in ('9','9.dng'),result['decoder_version']
                        assert raster['semantics']=='display_referred_srgb_8bit'
                        receipt['decoder_version']=result['decoder_version']
                        receipt['input']={'name':photo.name,'sha256':hashlib.sha256(photo.read_bytes()).hexdigest()}
                    if operation=='raw_render' and args.sdk:
                        credential=temp/'token';credential.write_text(token);credential.chmod(0o600)
                        sdk=subprocess.run([str(args.sdk),'--endpoint',base,str(credential),str(photo),'raw_render'],capture_output=True,text=True,timeout=125,env=env)
                        assert sdk.returncode==0,sdk.stderr
                        sdk_result=json.loads(sdk.stdout)
                        assert sdk_result['result']['decoder_version']==result['decoder_version']
                        receipt['sdk_result']=sdk_result
                    report['checks'].append(receipt);print(json.dumps(receipt),flush=True)
                    args.report.write_text(json.dumps(report,indent=2)+'\n')
                if args.unsupported_raw:
                    status,value,elapsed=call({'operation':'raw_render','exposure':0,'noise_reduction':1},args.unsupported_raw)
                    assert status==400 and value['error']['code']=='upstream_invalid_request',(status,value)
                    assert call({'operation':'ocr'})[0]==200
                    report['checks'].append({'check':'unsupported_raw9_rejected_without_fallback','status':status,'error':value})
                if args.sdk:
                    credential=temp/'token';credential.write_text(token);credential.chmod(0o600)
                    sdk=subprocess.run([str(args.sdk),'--endpoint',base,str(credential),str(fixture),'ocr'],capture_output=True,text=True,timeout=125,env=env)
                    assert sdk.returncode==0,sdk.stderr
                    receipt=json.loads(sdk.stdout);assert 'INFER' in ' '.join(x['text'] for x in receipt['result']['lines']).upper()
                    report['checks'].append({'check':'negotiated_sdk_consumer','result':receipt})
            finally:
                process.terminate()
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.wait()
    args.report.write_text(json.dumps(report,indent=2)+'\n')
if __name__=='__main__':main()
