#!/usr/bin/env python3
"""Check an installed Ztty against a real XIM service on an owned private Xvfb."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import traceback

def wait(predicate, label, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (FileNotFoundError, ConnectionRefusedError):
            pass
        time.sleep(.025)
    raise TimeoutError(label)

def descendants():
    result, queue = {}, [os.getpid()]
    while queue:
        parent = queue.pop()
        try:
            children = Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
        except FileNotFoundError:
            continue
        for raw in children:
            pid = int(raw)
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                if int(fields[1]) == parent and pid not in result:
                    result[pid] = fields[19]
                    queue.append(pid)
            except FileNotFoundError:
                pass
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ztty", type=Path, required=True)
    parser.add_argument("--ztty-source", type=Path, required=True)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--server-mode", choices=("probe", "ztype"), default="probe")
    parser.add_argument("--name", default="fcitx")
    parser.add_argument("--dictionary", type=Path)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--expect-commit", nargs=2, action="append", default=[],
                        metavar=("KEYS", "TEXT"), help="check an additional native PTY commit")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    sys.path.insert(0, str(args.ztty_source / "tools"))
    sys.path.insert(0, str(args.ztty_source / "tests"))
    from isolated_xvfb import start_xvfb
    from input_gui import CHILD
    from Xlib import X, XK, display
    from Xlib.ext import xtest
    report = {"ok":False, "scope":"installed Ztty, owned Xvfb, XTEST keys, XIM commit and raw PTY",
              "ztty":str(args.ztty.resolve()), "server":str(args.server.resolve()),
              "server_mode":args.server_mode, "xim_name":args.name, "checks":[],
              "server_sha256":hashlib.sha256(args.server.read_bytes()).hexdigest(),
              "ztty_sha256":hashlib.sha256(args.ztty.read_bytes()).hexdigest()}
    if args.dictionary:
        report["dictionary_sha256"]=hashlib.sha256(args.dictionary.read_bytes()).hexdigest()
    assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0
    xvfb = connection = server = None
    report_path = args.output.resolve()
    with tempfile.TemporaryDirectory(prefix="ztype-ztty-xim-", dir="/var/tmp") as temporary:
        test_root = Path(temporary)
        log = (test_root/"process.log").open("wb")
        try:
            xvfb = start_xvfb(screen="1000x700x24", stderr=log)
            report["private_display"] = xvfb.evidence
            connection = display.Display(xvfb.display)
            assert connection.has_extension("XTEST")
            for folder in ("config", "data", "state", "cache", "runtime"):
                (test_root/folder).mkdir(mode=0o700)
            env = dict(os.environ, DISPLAY=xvfb.display, XMODIFIERS="@im="+args.name,
                       ZTYPE_XIM_NAME=args.name, LC_ALL="C.UTF-8",
                       XDG_CONFIG_HOME=str(test_root/"config"), XDG_DATA_HOME=str(test_root/"data"),
                       XDG_STATE_HOME=str(test_root/"state"), XDG_CACHE_HOME=str(test_root/"cache"),
                       XDG_RUNTIME_DIR=str(test_root/"runtime"),
                       ZTTY_SOCKET="@ztype-xim-"+test_root.name)
            for key in ("WAYLAND_DISPLAY","DBUS_SESSION_BUS_ADDRESS","XAUTHORITY","LD_PRELOAD"):
                env.pop(key, None)
            config = test_root/"ztty.conf"
            config.write_text("font = monospace:size=11\nbackground = #000000\ncursor-blink = false\nrenderer = software\n")
            selection = connection.intern_atom("@server="+args.name)
            def owner():
                value = connection.get_selection_owner(selection)
                return value.id if value else 0
            def rpc(request):
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(5)
                    channel.connect("\0"+env["ZTTY_SOCKET"][1:])
                    channel.sendall(json.dumps(request).encode()+b"\n")
                    data = b""
                    while b"\n" not in data:
                        block = channel.recv(65536)
                        if not block:
                            raise EOFError("private broker EOF")
                        data += block
                        if len(data)>1048576:
                            raise RuntimeError("private broker response bound")
                result = json.loads(data.split(b"\n",1)[0])
                assert result["ok"], result
                return result["result"]
            def launch(name):
                record, control = test_root/(name+".bin"), test_root/(name+".sock")
                subprocess.Popen([str(args.ztty.resolve()),"--name",name,"--config",str(config),
                                  "--geometry","80x24","-e","/usr/bin/python3","-c",CHILD,
                                  str(record),str(control)],env=env,stdout=subprocess.DEVNULL,stderr=log,
                                  start_new_session=True)
                wait(control.exists, "raw PTY startup")
                info = rpc({"op":"info","session":name})
                return {"name":name,"record":record,"control":control,"info":info,
                        "view":connection.create_resource_object("window",info["window_id"])}
            def barrier(window, clear=False):
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as channel:
                    channel.settimeout(8)
                    channel.connect(str(window["control"]))
                    channel.sendall(json.dumps({"clear":clear}).encode()+b"\n")
                    assert json.loads(channel.recv(4096))["ok"]
            def focus(window):
                window["view"].set_input_focus(X.RevertToParent, X.CurrentTime)
                connection.sync()
                time.sleep(.08)
            def tap(name):
                code = connection.keysym_to_keycode(XK.string_to_keysym(name))
                assert code, name
                xtest.fake_input(connection,X.KeyPress,code)
                xtest.fake_input(connection,X.KeyRelease,code)
                connection.sync()
                time.sleep(.025)
            def keys(text):
                for letter in text:
                    tap("space" if letter==" " else letter)
            def start_server():
                nonlocal server
                command = [str(args.server.resolve())]
                if args.server_mode=="probe":
                    command.append(args.name)
                else:
                    command.append("x11")
                    if args.dictionary:
                        command.extend(["--dict",str(args.dictionary.resolve())])
                    if args.config:
                        command.extend(["--config",str(args.config.resolve())])
                server = subprocess.Popen(command,env=env,stdout=log,stderr=log,start_new_session=True)
                wait(lambda: owner() or (server.poll() is not None and (_ for _ in ()).throw(RuntimeError("XIM service exited"))),
                     "private XIM selection owner")
                time.sleep(.3)
            def stop_server():
                nonlocal server
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
                server = None
                wait(lambda:not owner(),"XIM owner release")
                time.sleep(.15)
            def check(label, window, text, expected):
                focus(window)
                barrier(window, True)
                keys(text)
                wait(lambda:window["record"].stat().st_size>0, "PTY input")
                time.sleep(.1)
                barrier(window)
                actual = window["record"].read_bytes()
                passed = actual == expected.encode()
                report["checks"].append({"name":label,"ok":passed,"actual":actual.decode("utf-8","replace"),"expected":expected})
                print(("PASS " if passed else "FAIL ")+label, flush=True)
                assert passed, label
            assert owner()==0
            first = launch("first")
            check("ASCII before XIM starts",first,"abc","abc")
            start_server()
            text, expected = ("t","你好世界") if args.server_mode=="probe" else ("nihao ","你好")
            check("existing Ztty reconnects after XIM starts",first,text,expected)
            check("multi-codepoint native commit reaches raw PTY",first,
                  "r" if args.server_mode=="probe" else "shijie ",
                  "かなカナ" if args.server_mode=="probe" else "世界")
            for extra_keys, extra_text in args.expect_commit:
                check("additional native commit: "+extra_keys.rstrip(),first,extra_keys,extra_text)
            second = launch("second")
            same_broker = first["info"]["broker_pid"]==second["info"]["broker_pid"]
            report["checks"].append({"name":"second window shares existing broker","ok":same_broker})
            assert same_broker
            check("second Ztty receives its own XIM commit",second,text,expected)
            check("focus returns to original terminal",first,text,expected)
            focus(first)
            barrier(first,True)
            keys("c" if args.server_mode=="probe" else "nihao")
            def visible_popup():
                for window in connection.screen().root.query_tree().children:
                    if window.get_wm_name()=="zinput-x11 candidate popup" and window.get_attributes().map_state==X.IsViewable:
                        return window
                return None
            popup=wait(visible_popup,"visible XIM candidate window")
            geometry=dict(line.split("=",1) for line in subprocess.check_output(
                ["xdotool","getwindowgeometry","--shell",str(popup.id)],env=env,text=True).splitlines())
            screen=connection.screen()
            x,y,width,height=(int(geometry[k]) for k in ("X","Y","WIDTH","HEIGHT"))
            assert width>0 and height>0 and x>=0 and y>=0
            assert x+width<=screen.width_in_pixels and y+height<=screen.height_in_pixels
            report["checks"].append({"name":"candidate popup is visible within private screen","ok":True,
                                      "geometry":[x,y,width,height]})
            if args.screenshot:
                from PIL import Image
                def capture_screen():
                    raw=screen.root.get_image(0,0,screen.width_in_pixels,screen.height_in_pixels,X.ZPixmap,0xffffffff)
                    return Image.frombytes("RGB",(screen.width_in_pixels,screen.height_in_pixels),raw.data,"raw","BGRX")
                if args.server_mode=="ztype":
                    # Mapping precedes the asynchronous glyph redraw. Capture the painted
                    # root window, so a blank or occluded popup cannot pass as visible.
                    def rendered_popup():
                        frame=capture_screen()
                        crop=frame.crop((x,y,x+width,y+height))
                        colors=crop.getcolors(width*height) or []
                        bright=sum(count for count,color in colors if min(color)>170)
                        return frame if bright>50 and len(colors)>32 else None
                    pixels=wait(rendered_popup,"candidate text painted on private screen")
                    report["checks"].append({"name":"candidate popup text is painted on screen","ok":True})
                else:
                    pixels=capture_screen()
                args.screenshot.parent.mkdir(parents=True,exist_ok=True)
                pixels.save(args.screenshot)
                report["screenshot"]=str(args.screenshot.resolve())
            keys("t") if args.server_mode=="probe" else tap("Escape")
            wait(lambda:not visible_popup(),"candidate popup hidden after completion")
            report["checks"].append({"name":"candidate popup hides after commit or cancellation","ok":True})
            stop_server()
            check("ordinary keys remain usable while XIM is stopped",first,"abc","abc")
            start_server()
            check("existing terminal reconnects after XIM restart",first,text,expected)
            rpc({"op":"close","session":"second"})
            rpc({"op":"close","session":"first"})
            stop_server()
            report["ok"]=all(item["ok"] for item in report["checks"])
        except Exception as error:
            report["error"]=str(error)
            report["traceback"]=traceback.format_exc()
        finally:
            if connection:
                connection.close()
            for sig in (signal.SIGTERM,signal.SIGKILL):
                for pid,token in descendants().items():
                    try:
                        current=Path(f"/proc/{pid}/stat").read_text().rsplit(")",1)[1].split()[19]
                        if current==token:
                            os.kill(pid,sig)
                    except (FileNotFoundError,ProcessLookupError):
                        pass
                deadline=time.monotonic()+1
                while time.monotonic()<deadline:
                    try:
                        pid,_=os.waitpid(-1,os.WNOHANG)
                    except ChildProcessError:
                        break
                    if pid==0:
                        time.sleep(.02)
            if xvfb:
                xvfb.close()
            report["owned_processes_remaining"]=len(descendants())
            log.close()
            report["process_log"]=(test_root/"process.log").read_text(errors="replace")[-8000:]
            report_path.parent.mkdir(parents=True,exist_ok=True)
            report_path.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps({"ok":report["ok"],"passed":sum(x["ok"] for x in report["checks"]),
                      "total":len(report["checks"]),"output":str(report_path),"error":report.get("error")},ensure_ascii=False))
    return 0 if report["ok"] and not report["owned_processes_remaining"] else 1

if __name__=="__main__":
    raise SystemExit(main())
