import base64, ctypes, json, os, queue, secrets, shutil, subprocess, time, urllib.request, tkinter as tk
from pathlib import Path
from tkinter import ttk, filedialog, messagebox

APP = "Project Stratagem"
DATA = Path(os.getenv("LOCALAPPDATA", str(Path.home()))) / "ProjectStratagem"
LOGS = DATA / "logs"
CFG = DATA / "settings.json"
TOK = DATA / "token.dpapi"
DATA.mkdir(parents=True, exist_ok=True); LOGS.mkdir(exist_ok=True)

def dpapi(data, protect=True):
    class Blob(ctypes.Structure):
        _fields_=[("cbData",ctypes.c_uint32),("pbData",ctypes.POINTER(ctypes.c_ubyte))]
    c=ctypes.windll.crypt32; k=ctypes.windll.kernel32
    k.LocalFree.restype=ctypes.c_void_p
    buf=ctypes.create_string_buffer(data); src=Blob(len(data),ctypes.cast(buf,ctypes.POINTER(ctypes.c_ubyte))); dst=Blob()
    fn=c.CryptProtectData if protect else c.CryptUnprotectData
    if not fn(ctypes.byref(src),None,None,None,None,0,ctypes.byref(dst)): raise ctypes.WinError()
    try: return ctypes.string_at(dst.pbData,dst.cbData)
    finally: k.LocalFree(dst.pbData)

def findexe(n): return shutil.which(n) or shutil.which(n+".exe")
def bridge_default():
    paths=[Path.home()/"Downloads/ollama-gateway-setup/ollama-gateway-setup/bridge.mjs",Path.home()/"Downloads/ollama-gateway-setup/bridge.mjs"]
    return str(next((p for p in paths if p.exists()),paths[0]))

class App:
 def __init__(self,root):
  self.root=root;root.title(APP);root.geometry("890x660");root.minsize(800,610);root.configure(bg="#12161d")
  self.procs={};self.queue=queue.Queue()
  self.cfg={"bridge":bridge_default(),"node":findexe("node") or "","cloudflared":findexe("cloudflared") or "","token_env":"BRIDGE_TOKEN","target":"http://127.0.0.1:11435","ollama":"http://127.0.0.1:11434"}
  try:self.cfg.update(json.loads(CFG.read_text()))
  except:pass
  try:self.token=dpapi(TOK.read_bytes(),False).decode()
  except:self.token=""
  self.ui();self.status();root.after(250,self.drain);root.protocol("WM_DELETE_WINDOW",self.close)
 def log(self,msg):
  line=f"[{time.strftime('%H:%M:%S')}] {msg}";self.queue.put(line)
  try:
   with (LOGS/"stratagem.log").open("a",encoding="utf8") as f:f.write(line+"\n")
  except:pass
 def ui(self):
  style=ttk.Style();style.theme_use("clam");style.configure("TFrame",background="#12161d");style.configure("TLabel",background="#12161d",foreground="#dce4ef");style.configure("TButton",padding=6);style.configure("TLabelframe",background="#12161d",foreground="#dce4ef");style.configure("TLabelframe.Label",background="#12161d",foreground="#dce4ef")
  main=ttk.Frame(self.root,padding=15);main.pack(fill="both",expand=True)
  ttk.Label(main,text="PROJECT STRATAGEM",font=("Segoe UI",20,"bold")).pack(anchor="w")
  ttk.Label(main,text="LOCAL AI GATEWAY CONTROL",foreground="#93a1b4").pack(anchor="w",pady=(0,12))
  st=ttk.Frame(main);st.pack(fill="x",pady=3);self.labels={}
  for name in ["Ollama · 11434","Bridge · 11435","Cloudflare Tunnel"]:
   card=tk.Frame(st,bg="#232a35",padx=12,pady=10);card.pack(side="left",expand=True,fill="x",padx=(0,6))
   tk.Label(card,text=name.upper(),bg="#232a35",fg="#9bacbf",font=("Segoe UI",9,"bold")).pack(anchor="w")
   l=tk.Label(card,text="CHECKING",bg="#232a35",fg="#f0c56e",font=("Segoe UI",10,"bold"));l.pack(anchor="w",pady=(4,0));self.labels[name]=l
  actions=ttk.Frame(main);actions.pack(fill="x",pady=10)
  self.btn(actions,"START ALL",self.start_all,"#20784f").pack(side="left",padx=(0,6)
  )
  self.btn(actions,"STOP ALL",self.stop_all,"#a33d47").pack(side="left",padx=6)
  self.btn(actions,"TEST OLLAMA",self.test).pack(side="left",padx=6)
  self.btn(actions,"GENERATE TOKEN",self.generate).pack(side="left",padx=6)
  self.btn(actions,"SAVE TOKEN",self.save_token).pack(side="left",padx=6)
  box=ttk.LabelFrame(main,text="Settings",padding=9);box.pack(fill="x",pady=(0,10))
  self.v={k:tk.StringVar(value=self.cfg.get(k,"")) for k in ["bridge","node","cloudflared","token_env","target","ollama"]}
  self.row(box,"bridge.mjs path",self.v["bridge"],True)
  self.row(box,"Node executable",self.v["node"],True)
  self.row(box,"cloudflared.exe",self.v["cloudflared"],True)
  for k,label in [("token_env","Token environment variable"),("target","Tunnel local URL"),("ollama","Ollama API URL")]:self.row(box,label,self.v[k])
  tr=ttk.Frame(box);tr.pack(fill="x",pady=3);ttk.Label(tr,text="Bridge token",width=23).pack(side="left")
  self.tv=tk.StringVar(value=self.token);self.token_entry=ttk.Entry(tr,textvariable=self.tv,show="•");self.token_entry.pack(side="left",fill="x",expand=True)
  self.btn(tr,"Show/Hide",self.toggle).pack(side="left",padx=5)
  self.btn(box,"SAVE SETTINGS",self.save_settings).pack(anchor="e",pady=(5,0))
  logs=ttk.LabelFrame(main,text="Activity log",padding=8);logs.pack(fill="both",expand=True)
  self.logtext=tk.Text(logs,bg="#0b0f14",fg="#cbd6e4",height=9,font=("Consolas",9),wrap="word",relief="flat");self.logtext.pack(fill="both",expand=True,side="left")
  sb=ttk.Scrollbar(logs,command=self.logtext.yview);sb.pack(side="right",fill="y");self.logtext.configure(yscrollcommand=sb.set)
  foot=ttk.Frame(main);foot.pack(fill="x",pady=(8,0));self.btn(foot,"OPEN LOGS",lambda:os.startfile(str(LOGS))).pack(side="left");self.btn(foot,"OPEN BRIDGE FOLDER",lambda:os.startfile(str(Path(self.v["bridge"].get()).parent))).pack(side="left",padx=5)
 def btn(self,parent,text,fn,bg="#303c4d"):
  return tk.Button(parent,text=text,command=fn,bg=bg,fg="white",activebackground="#40536b",activeforeground="white",relief="flat",padx=10,pady=6,font=("Segoe UI",9,"bold"),cursor="hand2")
 def row(self,parent,label,var,browse=False):
  f=ttk.Frame(parent);f.pack(fill="x",pady=2);ttk.Label(f,text=label,width=23).pack(side="left");ttk.Entry(f,textvariable=var).pack(side="left",fill="x",expand=True)
  if browse:self.btn(f,"Browse",lambda:self.pick(var,label)).pack(side="left",padx=5)
 def pick(self,var,label):
  p=filedialog.askopenfilename(title=f"Choose {label}",filetypes=[("Scripts and executables","*.mjs *.js *.exe"),("All files","*.*")])
  if p:var.set(p)
 def toggle(self):self.token_entry.configure(show="" if self.token_entry.cget("show") else "•")
 def generate(self):
  self.tv.set(secrets.token_urlsafe(32));self.log("New token generated. Save it and ensure the bridge/Worker uses the matching value.")
 def save_token(self):
  t=self.tv.get().strip()
  if not t:messagebox.showwarning(APP,"Token is blank.");return
  try:TOK.write_bytes(dpapi(t.encode(),True));self.token=t;self.log("Token stored encrypted with Windows DPAPI (current user).")
  except Exception as e:messagebox.showerror(APP,"Secure token storage failed: "+str(e))
 def save_settings(self):
  self.cfg={k:v.get().strip() for k,v in self.v.items()};CFG.write_text(json.dumps(self.cfg,indent=2),encoding="utf8");self.log("Settings saved.")
 def drain(self):
  try:
   while True:self.logtext.insert("end",self.queue.get_nowait()+"\n");self.logtext.see("end")
  except queue.Empty:pass
  self.root.after(250,self.drain)
 def port(self,p):
  import socket
  try:s=socket.create_connection(("127.0.0.1",p),timeout=.3);s.close();return True
  except:return False
 def status(self):
  for key,p in [("Ollama · 11434",11434),("Bridge · 11435",11435)]:
   ok=self.port(p);self.labels[key].config(text="ONLINE" if ok else "OFFLINE",fg="#58d18b" if ok else "#e06b72")
  t=self.procs.get("tunnel");ok=bool(t and t.poll() is None);self.labels["Cloudflare Tunnel"].config(text="RUNNING" if ok else "STOPPED",fg="#58d18b" if ok else "#e06b72")
  self.root.after(2500,self.status)
 def start(self,key,cmd,cwd=None,env=None):
  p=self.procs.get(key)
  if p and p.poll() is None:self.log(f"{key} already started by Stratagem.");return
  f=(LOGS/f"{key}.log").open("a",encoding="utf8")
  try:self.procs[key]=subprocess.Popen(cmd,cwd=cwd,env=env,stdout=f,stderr=subprocess.STDOUT,creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0));self.log(f"Started {key} (PID {self.procs[key].pid}). Logs: {key}.log")
  except Exception as e:f.close();self.log(f"Launch failed ({key}): {e}")
 def start_ollama(self):
  if self.port(11434):self.log("Ollama already running on localhost:11434.");return
  exe=findexe("ollama")
  if not exe:messagebox.showerror(APP,"Ollama not found in PATH. Install it first.");return
  self.start("ollama",[exe,"serve"])
 def start_bridge(self):
  if self.port(11435):self.log("Port 11435 occupied; avoiding duplicate bridge.");return
  script=Path(self.v["bridge"].get());node=self.v["node"].get().strip() or findexe("node");token=self.tv.get().strip();env=os.environ.copy();env[self.v["token_env"].get().strip() or "BRIDGE_TOKEN"]=token
  if not script.is_file():messagebox.showerror(APP,f"bridge.mjs not found:\n{script}\nUse Browse to select it.");return
  if not node:messagebox.showerror(APP,"Node.js missing. Install it or select node.exe.");return
  if not token:messagebox.showerror(APP,"Token missing. Generate or paste one, then Save Token.");return
  self.start("bridge",[node,str(script)],str(script.parent),env)
 def start_tunnel(self):
  p=self.procs.get("tunnel")
  if p and p.poll() is None:self.log("Tunnel already running.");return
  exe=self.v["cloudflared"].get().strip() or findexe("cloudflared")
  if not exe:messagebox.showerror(APP,"cloudflared missing. Install Cloudflare Tunnel or select cloudflared.exe.");return
  self.start("tunnel",[exe,"tunnel","--url",self.v["target"].get().strip()])
  self.log("Quick Tunnel creates a temporary URL. See tunnel.log. This app does not edit Cloudflare Worker settings.")
 def start_all(self):
  self.save_settings();self.start_ollama();self.root.after(1200,self.start_bridge);self.root.after(2500,self.start_tunnel)
 def stop_all(self):
  for key in ["tunnel","bridge","ollama"]:
   p=self.procs.get(key)
   if p and p.poll() is None:
    try:p.terminate();self.log(f"Stop requested for {key} PID {p.pid}.")
    except Exception as e:self.log(f"Stop error {key}: {e}")
  self.log("Only processes launched by this app are stopped. Existing Windows services remain running.")
 def test(self):
  url=self.v["ollama"].get().rstrip("/")+"/api/tags"
  try:
   with urllib.request.urlopen(url,timeout=5) as r:data=json.loads(r.read().decode())
   models=[m.get("name","?") for m in data.get("models",[])]
   self.log("Ollama API OK. Models: "+(", ".join(models) if models else "none found"))
  except Exception as e:self.log("Ollama API test failed: "+str(e))
 def close(self):
  self.root.destroy()
def main():
 if os.name!="nt":messagebox.showerror(APP,"Windows only");return
 root=tk.Tk();App(root);root.mainloop()
if __name__=="__main__":main()
