import json, os, glob, collections

YGG_ROOT = os.path.expanduser("~/.ygg/sessions")
d=os.path.join(YGG_ROOT,"c4c8e202815bca65")

files=sorted(glob.glob(os.path.join(d,"*.jsonl")), key=lambda f:-os.path.getsize(f))
print("largest 6 files:", [os.path.basename(f) for f in files[:6]])

# 1) largest entries in the biggest file + media scan across all files
big=files[0]
largest=[]
media_count=0; media_bytes=0; media_kinds=collections.Counter()
toolresult_bytes=collections.Counter()
n_entries=0
for f in files:
    for line in open(f, encoding="utf-8", errors="replace"):
        line=line.strip()
        if not line: continue
        try: e=json.loads(line)
        except: continue
        if e.get("type")!="entry": continue
        n_entries+=1
        v=e.get("value") if isinstance(e.get("value"),dict) else e
        if not isinstance(v,dict): continue
        for p in (v.get("message") or {}).get("parts",[]):
            if not isinstance(p,dict): continue
            pt=str(p.get("type"))
            if "media" in pt or "image" in pt or "audio" in pt:
                b=len(json.dumps(p)); media_count+=1; media_bytes+=b; media_kinds[pt]+=1
            elif "tool_result" in pt or "toolResult" in pt:
                toolresult_bytes[os.path.basename(f)]+=len(json.dumps(p))
        if f==big:
            largest.append((len(line), e))
largest.sort(key=lambda t: t[0], reverse=True)
print("entries scanned:", n_entries, "| media parts:", media_count, "bytes:", media_bytes, "kinds:", dict(media_kinds))
print("top-5 largest entries in biggest file:")
for b,e in largest[:5]:
    v=e.get("value") if isinstance(e.get("value"),dict) else e
    desc=str(v)[:180].replace("\n"," ")
    print("  %8d B  %s" % (b, desc))
print("top-5 files by tool-result bytes:", toolresult_bytes.most_common(5))

# 2) one full compaction-kind usage record + one checkpoint entry (boundary persistence?)
comp_rec=None; chk=None
for f in files:
    for line in open(f, encoding="utf-8", errors="replace"):
        try: e=json.loads(line)
        except: continue
        if e.get("type")=="usage" and (e.get("record",{}).get("kind",{}) or {}).get("kind")=="compaction" and comp_rec is None:
            comp_rec=e
        if e.get("type")=="checkpoint" and chk is None:
            chk=e
        if comp_rec is not None and chk is not None: break
    if comp_rec is not None and chk is not None: break
print()
print("compaction-kind USAGE record (full):", json.dumps(comp_rec)[:600] if comp_rec else None)
print()
print("checkpoint entry (full):", json.dumps(chk)[:600] if chk else None)

# 3) cold-cache storms across the 30-day mother session (root only)
cold_n=0; cold_in=0; warm_n=0; warm_in=0; total_in=0; total_cr=0
first_cold_ts=None; last_ts=None
for f in sorted(glob.glob(os.path.join(d,"*.jsonl"))):
    for line in open(f, encoding="utf-8", errors="replace"):
        try: e=json.loads(line)
        except: continue
        if e.get("type")!="usage": continue
        rec=e.get("record",{}); kk=(rec.get("kind",{}) or {}).get("kind")
        if kk!="assistant_turn": continue
        u=rec.get("usage",{}); i=u.get("input_tokens",0); cr=u.get("cache_read_tokens",0)
        total_in+=i; total_cr+=cr
        ts=e.get("timestamp") or rec.get("timestamp")
        if ts:
            if first_cold_ts is None: first_cold_ts=ts
            last_ts=ts
        if cr==0:
            cold_n+=1; cold_in+=i
            if e.get("timestamp"):
                pass
        else:
            warm_n+=1; warm_in+=i
print()
print("COLD-CACHE (cache_read==0) root requests: n=%d of %d | fresh-input billed on cold=%d vs warm=%d" % (cold_n, cold_n+warm_n, cold_in, warm_in))
print("span: %s .. %s" % (first_cold_ts, last_ts))
print("cold share: cold requests = %.1f%% of requests but %.1f%% of fresh input" % (100.0*cold_n/max(1,cold_n+warm_n), 100.0*cold_in/max(1,total_in)))
