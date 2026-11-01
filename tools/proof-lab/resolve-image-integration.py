"""Merge KA2's actual plane transport with KD2's one shared image Launch reader."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

candidate=Path(sys.argv[1]).resolve(strict=True)
source=candidate/'source'
root=Path(__file__).resolve().parent
capture=root/'source-capture-ka2integration-ai3ts3kx/source'
repo=Path('/Users/brackly/Desktop/Projects/moruna-v1')
relative=Path('crates/moruna-sandbox/src/image')
record=[]

def write(name,data,reason):
    dest=source/name
    old=hashlib.sha256(dest.read_bytes()).hexdigest() if dest.exists() else None
    dest.parent.mkdir(parents=True,exist_ok=True)
    dest.write_text(data)
    record.append({'path':str(name),'before':old,'after':hashlib.sha256(dest.read_bytes()).hexdigest(),'reason':reason})

for path in (capture/relative).rglob('*'):
    if path.is_file():
        write(relative/path.relative_to(capture/relative),path.read_text(),'KA2 current plane transport and refusal fixes')
obsolete=source/relative/'output.rs'
if obsolete.exists():
    record.append({'path':str(relative/'output.rs'),'deleted':True,'reason':'KA2 supersedes old stream reader with plane transport'})
    obsolete.unlink()

name=relative/'mod.rs'
data=(source/name).read_text()
incoming=subprocess.check_output(['git','show','7a5c073:crates/moruna-sandbox/src/image/mod.rs'],cwd=repo,text=True)
def region(text,start,end):
    return text[text.index(start):text.index(end,text.index(start))]
data=data.replace('use moruna_image::Image;', 'use moruna_image::{Image, Launch};')
data=data.replace('/// The `PATH` a command is looked for in when the image\'s environment names none.\nconst DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";\n','')
start='/// The user an image runs as, from its config'
end='fn cstring('
data=data.replace(region(data,start,end),'')
start='/// The paths `command` is tried at:'
end='/// An image\'s command as a kernel.'
data=data.replace(region(data,start,end),'')
start='    uid: u32,\n'
end='    filters: Vec<BpfProgram>,'
data=data.replace(region(data,start,end),'    /// The single image-config reader shared with the native engine.\n    launch: Launch,\n')
data=data.replace('.field("command", &self.command)', '.field("command", &self.launch.command)')
start='        let command = command.unwrap_or_else'
end='        let rootfs = image'
replacement=region(incoming,'        let launch = moruna_image::Launch::new',end)
data=data.replace(region(data,start,end),replacement)
data=data.replace('"image": image.digest.to_string(), "command": command, "env": env,\n            "working_dir": working_dir, "user": image.config.user, "plane": plane,',
                  '"image": image.digest.to_string(), "command": &launch.command, "env": &launch.env,\n            "working_dir": &launch.working_dir, "user": image.config.user, "plane": plane,')
data=data.replace('            uid,\n            gid,\n            working_dir,\n            command,\n            env,\n', '            launch,\n')
start='        let plan = Plan {'
end='        // The stage\'s plane:'
replacement=region(incoming,start,'        let started = spawn')
data=data.replace(region(data,start,end),replacement)
assert 'fn identity(' not in data and 'fn environment(' not in data and 'fn candidates(' not in data
assert not re.search(r'^<<<<<<<|^=======|^>>>>>>>',data,re.M)
write(name,data,'preserve actual KA2 plane transport and exact KD2 shared Launch implementation')

name=relative/'tests.rs'
data=(source/name).read_text()
start='    // The user.\n'
end='    // How a process ends.\n'
assert data.count(start)==1 and data.count(end)==1
data=data.replace(region(data,start,end),'    // Image Launch owns and tests user, environment and command-path interpretation.\n')
write(name,data,'retain KA2 record/transport tests; shared Launch semantics tested in moruna-image')
out=candidate/'image-resolution.json'
assert not out.exists()
out.write_text(json.dumps({'state':'integration source only; compile/tests pending','resolutions':record},indent=2)+'\n')
print(json.dumps({'candidate':str(candidate),'files':len(record)}))
