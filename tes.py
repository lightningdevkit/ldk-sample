import os
import subprocess
proc = subprocess.Popen(
    [
    "./target/debug/ldk-sample",
    "{}:{}@127.0.0.1:{}".format("polaruser","polarpass","18443"),
    "/.",
    "8781",
    "regtest",
    ],  
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    preexec_fn=os.setsid
)

proc.stdout.readline()
proc.stdout.readline()
node_id=proc.stdout.readline().decode("utf-8")[17:83]
running = True
print(node_id)
proc.stdin.write(bytes("exit\n", 'utf-8'))
        

from typing import Dict, Any, Callable, List, Optional, cast

opts = "DataLossProtect: supported, InitialRoutingSync: not supported, UpfrontShutdownScript: supported, GossipQueries: not supported, VariableLengthOnion: required, StaticRemoteKey: required, PaymentSecret: required, BasicMPP: supported, Wumbo: supported, AnchorsZeroFeeHtlcTx: not supported, ShutdownAnySegwit: supported, OnionMessages: not supported, ChannelType: supported, SCIDPrivacy: supported, ZeroConf: supported, unknown flags: none".split(', ')
options: Dict[str, str] = {}
val: Dict[str, str] = {}
val['supported'] = 'odd'
val['required'] = 'even'

for o in opts:
    k, v = o.split(': ')
    print(k,v)
    if v != 'not supported':
        if(k == 'DataLossProtect'):
            options['option_data_loss_protect'] = val[v]
        elif(k == 'UpfrontShutdownScript'):
            options['option_upfront_shutdown_script'] = val[v]
        elif(k == 'GossipQueries'):
            options['option_gossip_queries'] = val[v]
        elif(k == 'VariableLengthOnion'):
            options['option_var_onion_optin'] = val[v]
        elif(k == 'StaticRemoteKey'):
            options['option_static_remotekey'] = val[v]
        elif(k == 'PaymentSecret'):
            options['option_payment_secret'] = val[v]
        elif(k == 'BasicMPP'):
            options['option_basic_mpp'] = val[v]
        elif(k == 'ShutdownAnySegwit'):
            options['option_shutdown_anysegwit'] = val[v]
        elif(k == 'ChannelType' and val[v] == 'odd'):
            options['supports_open_accept_channel_type'] = 'true'
        elif(k == 'SCIDPrivacy'):
            options['option_scid_alias'] = val[v]
        elif(k == 'Keysend'):
            options['option_keysend'] = val[v]
        else:
            options[k] = None

print(options)
