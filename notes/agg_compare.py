import json, os, glob, collections

PI_ROOT = os.path.expanduser("~/.pi/agent/sessions")
YGG_ROOT = os.path.expanduser("~/.ygg/sessions")

def load(path):
    out=[]
    for line in open(path, encoding="utf-8", errors="replace"):
        line=line.strip()
        if not line: continue
        try: out.append(json.loads(line))
        except: pass
    return out

def load_dir(d):
    ents=[]
    for f in glob.glob(os.path.join(d,"*.jsonl")):
        for line in open(f, encoding="utf-8", errors="replace"):
            line=line.strip()
            if not line: continue
            try: ents.append(json.loads(line))
            except: pass
    return ents

def byid(ents):
    return {e["id"]: i for i,e in enumerate(ents) if "id" in e}

# ---- PI totals (all sessions) ----
pi_files=[]
for d in sorted(glob.glob(os.path.join(PI_ROOT,"*"))):
    for f in sorted(glob.glob(os.path.join(d,"*.jsonl"))):
        if os.path.getsize(f) < 10000: continue
        pi_files.append((d,f))
tot=collections.defaultdict(int)
for d,f in pi_files:
    ents=load(f)
    for e in ents:
        if e.get("type")!="message": continue
        u=(e.get("message") or {}).get("usage")
        if not u: continue
        tot["req"]+=1; tot["in"]+=u.get("input",0); tot["cacheR"]+=u.get("cacheRead",0)
        tot["cacheW"]+=u.get("cacheWrite",0); tot["out"]+=u.get("output",0)
        tot["rsn"]+=u.get("reasoning",0); tot["cost"]+= (u.get("cost") or {}).get("total",0)
print("PI FLEET TOTALS: files=%d req=%d prompt=%d (fresh=%d + cacheR=%d = %.1f%% cached) out=%d reasoning=%d (%.0f%% of out) cost=$%.0f" % (
    len(pi_files), tot["req"], tot["in"]+tot["cacheR"], tot["in"], tot["cacheR"],
    100.0*tot["cacheR"]/max(1,tot["in"]+tot["cacheR"]), tot["out"], tot["rsn"],
    100.0*tot["rsn"]/max(1,tot["out"]), tot["cost"]))

# ---- PI flagship: Aug 17 13.6MB codex-spark xhigh ----
F=os.path.join(PI_ROOT,"--Users-achumukundan-github-skaft-software-ygg--","2026-08-17T23-16-13-542Z_01a01202-e066-7ccc-92cf-2ce9f89adcc6.jsonl")
ents=load(F)
idx=byid(ents)
comps=[e for e in ents if e.get("type")=="compaction"]
start=idx.get(comps[-1].get("firstKeptEntryId"), len(ents)) if comps else len(ents)
tot2=collections.Counter(); prev_tc=False
for e in ents[start:]:
    if e.get("type")!="message": continue
    m=e.get("message",{}); role=m.get("role"); c=m.get("content")
    if not isinstance(c,list):
        if isinstance(c,str): tot2["user_text" if role=="user" else "assistant_text"]+=len(c)
        prev_tc = role=="assistant"
        continue
    any_tc=False
    for p in c:
        if not isinstance(p,dict): continue
        t=p.get("type")
        if t=="thinking":
            tot2["thinking_text"]+=len(p.get("thinking","") or "")
            tot2["thinking_sig"]+=len(p.get("thinkingSignature","") or "")
        elif t=="toolCall":
            tot2["toolCall"]+=len(json.dumps(p)); any_tc=True
        elif t=="text":
            if role=="user" and prev_tc: tot2["toolResult_user_text"]+=len(p.get("text","") or "")
            elif role=="user": tot2["user_text"]+=len(p.get("text","") or "")
            else: tot2["assistant_text"]+=len(p.get("text","") or "")
        else: tot2["other_%s"%t]+=len(json.dumps(p))
    prev_tc = (role=="assistant" and any_tc)
totb=sum(tot2.values()) or 1
print()
print("PI flagship (Aug17, 13.6MB, %d compactions) post-last-compaction composition:" % len(comps))
for k,v in tot2.most_common():
    print("   %-24s %8d B (%4.1f%%)" % (k,v,100.0*v/totb))
tb=[e.get("tokensBefore",0) for e in comps]
cu=sum((e.get("usage") or {}).get("total",0) for e in comps)
print("   compactions: n=%d tokensBefore=%s sum-usage=$%.2f" % (len(comps), [int(x) for x in tb], cu))

# ---- YGG side ----
print()
print("="*78); print("YGG ROOT SESSIONS (window)"); print("="*78)
for sid in ["c4c8e202815bca65","43769f70c3a939ad","451dab6dfa25c90","451ddb6dfa261a9","e2590490b9ca47ed","57f5e5307bf6638d"]:
    d=os.path.join(YGG_ROOT,sid)
    if not os.path.isdir(d): continue
    ents=load_dir(d)
    kinds=collections.Counter(); sums=collections.defaultdict(lambda: [0,0,0,0,0,0])
    for e in ents:
        if e.get("type")!="usage": continue
        rec=e.get("record",{}); kk=(rec.get("kind") or {}).get("kind","?")
        kinds[kk]+=1; a=sums[kk]; a[0]+=1
        u=rec.get("usage",{})
        a[1]+=u.get("input_tokens",0); a[2]+=u.get("cache_read_tokens",0)
        a[3]+=u.get("cache_write_tokens",0); a[4]+=u.get("output_tokens",0)
        a[5]+=u.get("reasoning_tokens",0)
    ncomp=len([e for e in ents if e.get("type")=="compaction"])
    print("--- %s: kinds=%s compactions=%d" % (sid, dict(kinds), ncomp))
    for k,(n,i,cr,cw,o,r) in sorted(sums.items()):
        print("    %-24s n=%6d in=%10d cR=%10d cW=%9d out=%8d rsn=%8d" % (k,n,i,cr,cw,o,r))

# Mother session root-only + composition
d=os.path.join(YGG_ROOT,"c4c8e202815bca65")
ents=load_dir(d)
idx=byid(ents)
comps=[e for e in ents if e.get("type")=="compaction"]
print()
print("Ygg mother session c4c8e202815bca65: compactions=%d" % len(comps))
for c in comps[-3:]:
    print("   compaction first_kept=%s ts=%s" % (c.get("first_kept"), c.get("timestamp")))
root_prompt=[]; root_out=0; root_rsn=0
for e in ents:
    if e.get("type")!="usage": continue
    rec=e.get("record",{}); kk=(rec.get("kind") or {}).get("kind")
    if kk!="assistant_turn": continue
    u=rec.get("usage",{})
    root_prompt.append(u.get("input_tokens",0)+u.get("cache_read_tokens",0))
    root_out+=u.get("output_tokens",0); root_rsn+=u.get("reasoning_tokens",0)
if root_prompt:
    print("  ROOT-ONLY: n=%d sumPrompt=%d maxPrompt=%d avgPrompt=%d out=%d rsn=%d (%.0f%% of out)" % (
        len(root_prompt), sum(root_prompt), max(root_prompt), sum(root_prompt)//max(1,len(root_prompt)), root_out, root_rsn, 100.0*root_rsn/max(1,root_out)))
if comps and (comps[-1].get("first_kept") in idx):
    start=idx[comps[-1]["first_kept"]]
else:
    start=len(ents)
tot3=collections.Counter()
for e in ents[start:]:
    if e.get("type")!="entry": continue
    v=e.get("value") if isinstance(e.get("value"),dict) else e
    if not isinstance(v,dict) or v.get("type")!="message": continue
    role=(v.get("message") or {}).get("role")
    for p in (v.get("message") or {}).get("parts",[]):
        if not isinstance(p,dict): continue
        pt=p.get("type"); pt=pt if isinstance(pt,str) else json.dumps(pt)
        b=len(json.dumps(p))
        if "reasoning" in str(pt): tot3["reasoning"]+=b
        elif "tool_result" in str(pt) or "toolResult" in str(pt) or "tool_output" in str(pt): tot3["tool_result"]+=b
        elif "tool_call" in str(pt) or "toolCall" in str(pt): tot3["tool_call"]+=b
        else: tot3["text_%s"%role]+=b
totb=sum(tot3.values()) or 1
print("  end-of-life (post-last-compaction) composition: %s (total %d B)" % (
    ", ".join("%s=%.1f%%" % (k,100.0*v/totb) for k,v in tot3.most_common()), totb))

# 57f5e5307bf6638d (Aug 23-24)
d2=os.path.join(YGG_ROOT,"57f5e5307bf6638d")
ents2=load_dir(d2)
kinds=collections.Counter(); sums=collections.defaultdict(lambda: [0,0,0,0,0,0]); root_prompt=[]
for e in ents2:
    if e.get("type")!="usage": continue
    rec=e.get("record",{}); kk=(rec.get("kind") or {}).get("kind","?")
    kinds[kk]+=1; a=sums[kk]; a[0]+=1
    u=rec.get("usage",{})
    a[1]+=u.get("input_tokens",0); a[2]+=u.get("cache_read_tokens",0)
    a[3]+=u.get("cache_write_tokens",0); a[4]+=u.get("output_tokens",0)
    a[5]+=u.get("reasoning_tokens",0)
    if kk=="assistant_turn": root_prompt.append(u.get("input_tokens",0)+u.get("cache_read_tokens",0))
print()
print("57f5e5307bf6638d (Aug23-24 swarm day): kinds=%s" % dict(kinds))
for k,(n,i,cr,cw,o,r) in sorted(sums.items()):
    print("    %-24s n=%6d in=%10d cR=%10d cW=%9d out=%8d rsn=%8d" % (k,n,i,cr,cw,o,r))
if root_prompt:
    print("  ROOT-ONLY: n=%d sumPrompt=%d maxPrompt=%d" % (len(root_prompt), sum(root_prompt), max(root_prompt)))
