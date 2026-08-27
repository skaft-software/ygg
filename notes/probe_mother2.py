import json, os, glob, collections, datetime
d=os.path.expanduser("~/.ygg/sessions/c4c8e202815bca65")
files=sorted(glob.glob(os.path.join(d,"*.jsonl")))
rows=[]
cost=collections.defaultdict(lambda: [0,0]); comp_inputs=[]; sessmax=0
for f in files:
    pref=os.path.basename(f).split("-")[0]
    try: day=datetime.datetime.utcfromtimestamp(int(pref)/1000).strftime("%m-%d")
    except Exception: day="??"
    maxtr=0; ntr=0
    for line in open(f, encoding="utf-8", errors="replace"):
        try: e=json.loads(line)
        except Exception: continue
        if e.get("type")=="entry":
            v=e.get("value") if isinstance(e.get("value"),dict) else e
            if isinstance(v,dict):
                msg=v.get("message") or {}
                for rolekey in ("User","Assistant"):
                    rm=msg.get(rolekey) or {}
                    for p in rm.get("content",[]):
                        if isinstance(p,dict) and "ToolResult" in p:
                            b=sum(len(json.dumps(x)) for x in p["ToolResult"].get("content",[]))
                            maxtr=max(maxtr,b); ntr+=1
        if e.get("type")=="usage":
            rec=e.get("record",{})
            kk=(rec.get("kind",{}) or {}).get("kind")
            cm=rec.get("cost_microdollars") or 0
            cost[kk][0]+=cm; cost[kk][1]+=1
            if kk=="compaction":
                comp_inputs.append((rec.get("usage",{}) or {}).get("input_tokens",0))
            sessmax=max(sessmax, rec.get("session_cost_microdollars") or 0)
    rows.append((day, os.path.basename(f)[:24], os.path.getsize(f)//1024, maxtr, ntr))

print("file timeline (date, file, sizeKB, maxToolResultKB, nToolResults):")
for day,fn,sz,mtr,ntr in sorted(rows):
    print("  %s %s %6dKB  maxTR=%6d  nTR=%4d" % (day, fn, sz, mtr//1024 if mtr else 0, ntr))
print()
for k,(c,n) in sorted(cost.items()):
    print("kind %-16s n=%6d  billed=$%.2f  avg=$%.4f/req" % (k,n,c/1e6,c/1e6/max(1,n)))
print("session total billed (last session_cost seen): $%.2f" % (sessmax/1e6))
ci=sorted(x for x in comp_inputs if x)
if ci:
    print("compaction input tokens: n=%d min=%d med=%d max=%d" % (len(ci),ci[0],ci[len(ci)//2],ci[-1]))
else:
    print("compaction input tokens: none")
