import type {
  ConnectedDevice,
  ContextUsage,
  HostBootstrap,
  SessionSnapshot,
  ThemeOption,
} from "./protocol";
import { PROTOCOL_VERSION } from "./protocol";

const at = (minutes: number) =>
  new Date(Date.UTC(2026, 6, 26, 16, minutes, 0)).toISOString();

const contextUsage = (tokens: number): ContextUsage => ({
  usage: {
    inputTokens: tokens,
    outputTokens: 0,
    contextTokens: tokens,
    contextLimit: 200_000,
  },
  compactions: 0,
  status: {
    current: {
      categories: tokens === 0 ? [] : [{ category: "other", tokens }],
      totalTokens: tokens,
    },
    updatedAtMs: 0,
  },
});

const rgb = (hex: string) => {
  const value = Number.parseInt(hex.slice(1), 16);
  return {
    kind: "rgb" as const,
    red: (value >> 16) & 255,
    green: (value >> 8) & 255,
    blue: value & 255,
  };
};

const themeCatalog: ThemeOption[] = [
  ["bone-machine", "Bone Machine", "#a93434", "compact"],
  ["circuit-garden", "Circuit Garden", "#00a87a", "airy"],
  ["field-notes", "Field Notes", "#6e7e35", "comfortable"],
  ["oxide-console", "Oxide Console", "#b45f32", "compact"],
  ["paper-ledger", "Paper Ledger", "#7d5c3b", "airy"],
  ["signal-noir", "Signal Noir", "#b52c3a", "comfortable"],
  ["synthwave-relay", "Synthwave Relay", "#d238b4", "compact"],
  ["tidepool", "Tidepool", "#168f91", "airy"],
  ["violet-hour", "Violet Hour", "#7650a5", "airy"],
  ["zen-mono", "Zen Mono", "#717171", "airy"],
].map(([id, name, accent, density], index) => ({
  id,
  theme: {
    name,
    source: "bundled",
    revision: index + 1,
    scheme: "unknown",
    density,
    motion: "full",
    typography: {
      body_family: "system-sans",
      mono_family: "system-mono",
      body_size: 17,
      display_ratio_milli: 1235,
    },
    colors: {
      accent: rgb(accent),
      foreground: rgb("#edf3f2"),
      muted: rgb("#778384"),
    },
    roles: {
      tool_title: {
        foreground: "accent",
        bold: true,
        dim: false,
        italic: false,
        underline: false,
        strikethrough: false,
      },
    },
  },
})) as ThemeOption[];

const devicesCatalog: ConnectedDevice[] = [
  {
    id: "device-this-mac",
    name: "Achu’s MacBook Pro",
    platform: "macOS",
    status: "this_device",
    lastSeen: "Now",
    connection: "local",
  },
  {
    id: "device-phone",
    name: "Achu’s iPhone",
    platform: "iOS",
    status: "connected",
    lastSeen: "Now",
    connection: "lan",
  },
  {
    id: "device-linux",
    name: "temper",
    platform: "Linux",
    status: "offline",
    lastSeen: "Yesterday at 11:42 PM",
    connection: "lan",
  },
];

const previewMarkup = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:">
    <style>
      * { box-sizing: border-box; }
      body { margin: 0; font-family: ui-sans-serif, system-ui, sans-serif; color: #102027; background: #f7faf8; }
      .page { min-height: 100vh; padding: 40px; background: radial-gradient(circle at top right, #d8f4ee, transparent 34%), #f7faf8; }
      .eyebrow { color: #168f91; font-size: 12px; font-weight: 700; letter-spacing: .12em; text-transform: uppercase; }
      h1 { max-width: 680px; margin: 12px 0 10px; font-size: clamp(30px, 5vw, 58px); line-height: .98; letter-spacing: -.05em; }
      p { color: #506268; max-width: 580px; line-height: 1.6; }
      .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 14px; margin-top: 32px; }
      .card { min-height: 140px; padding: 20px; border: 1px solid #dce8e5; border-radius: 16px; background: rgba(255,255,255,.82); box-shadow: 0 18px 50px rgba(21,70,69,.08); }
      .metric { font-size: 28px; font-weight: 700; margin-top: 28px; }
      .label { color: #6e7d82; font-size: 13px; }
      .bar { height: 7px; margin-top: 14px; border-radius: 10px; background: #e6efed; overflow: hidden; }
      .bar span { display: block; width: 78%; height: 100%; background: linear-gradient(90deg,#35c759,#16a89a,#397cf6); }
      @media (max-width: 600px) { .page { padding: 24px; } .grid { grid-template-columns: 1fr; } }
    </style>
  </head>
  <body>
    <main class="page">
      <div class="eyebrow">ygg release pulse</div>
      <h1>Everything important, ready for review.</h1>
      <p>A live summary of the release candidate, grounded in the checks and files consulted during this session.</p>
      <section class="grid">
        <article class="card"><div class="label">Focused checks</div><div class="metric">42 / 42</div><div class="bar"><span></span></div></article>
        <article class="card"><div class="label">Changed files</div><div class="metric">8</div><div class="bar"><span style="width:62%"></span></div></article>
        <article class="card"><div class="label">Open blockers</div><div class="metric">0</div><div class="bar"><span style="width:100%"></span></div></article>
      </section>
    </main>
  </body>
</html>`;

export const fixtureBootstrap: HostBootstrap = {
  protocolVersion: PROTOCOL_VERSION,
  host: {
    id: "host-macbook",
    name: "Achu’s MacBook Pro",
    connection: "local",
  },
  catalogRevision: 12,
  selectedSessionId: "session-fresh",
  projects: [
    {
      id: "project-ygg",
      name: "ygg",
      trusted: true,
      archived: false,
      available: true,
      isDefault: true,
      sessionCount: 4,
      liveSessionCount: 2,
    },
    {
      id: "project-notes",
      name: "Research notes",
      trusted: true,
      archived: false,
      available: true,
      isDefault: false,
      sessionCount: 1,
      liveSessionCount: 0,
    },
  ],
  sessions: [
    {
      id: "session-fresh",
      projectId: "project-ygg",
      title: "New session",
      preview: "Ready when you are",
      status: "idle",
      updatedAt: at(58),
      pinned: false,
      archived: false,
      lifecycle: "active",
      unread: false,
      modelId: "claude-sonnet-4-6",
      attentionCount: 0,
    },
    {
      id: "session-live",
      projectId: "project-ygg",
      title: "Refine onboarding preview",
      preview: "Checking the responsive states",
      status: "working",
      updatedAt: at(54),
      pinned: true,
      archived: false,
      lifecycle: "active",
      unread: false,
      modelId: "gpt-5.4",
      attentionCount: 0,
      pullRequest: { state: "in_progress" },
    },
    {
      id: "session-attention",
      projectId: "project-ygg",
      title: "Prepare signed macOS build",
      preview: "Needs access to the signing key",
      status: "needs_attention",
      updatedAt: at(48),
      pinned: false,
      archived: false,
      lifecycle: "active",
      unread: true,
      modelId: "claude-sonnet-4-6",
      attentionCount: 1,
      pullRequest: { state: "ready" },
    },
    {
      id: "session-done",
      projectId: "project-ygg",
      title: "Review release readiness",
      preview: "Release pulse is ready",
      status: "done",
      updatedAt: at(32),
      pinned: true,
      archived: false,
      lifecycle: "active",
      unread: true,
      modelId: "qwen3.5-27b",
      attentionCount: 0,
      pullRequest: { state: "merged" },
    },
    {
      id: "session-recent",
      projectId: "project-notes",
      title: "Summarize provider notes",
      preview: "Compared three local endpoints",
      status: "idle",
      updatedAt: at(12),
      pinned: false,
      archived: false,
      lifecycle: "active",
      unread: false,
      modelId: "qwen3.5-27b",
      attentionCount: 0,
    },
  ],
  models: [
    {
      id: "claude-sonnet-4-6",
      name: "Claude Sonnet 4.6",
      provider: "Anthropic",
      local: false,
      available: true,
      reasoning: ["low", "medium", "high"],
      defaultReasoning: "high",
      inputPricing: {
        baseMicrodollarsPerMillionTokens: 3_000_000,
        tiers: [
          {
            minInputTokens: 200_000,
            microdollarsPerMillionTokens: 6_000_000,
          },
        ],
      },
      inputModalities: ["text", "image", "document"],
    },
    {
      id: "gpt-5.4",
      name: "GPT-5.4",
      provider: "OpenAI",
      local: false,
      available: true,
      reasoning: ["low", "medium", "high"],
      defaultReasoning: "high",
      inputPricing: {
        baseMicrodollarsPerMillionTokens: 2_500_000,
        tiers: [],
      },
      inputModalities: ["text", "image", "audio", "document"],
    },
    {
      id: "qwen3.5-27b",
      name: "Qwen 3.5 27B",
      provider: "Local",
      local: true,
      available: true,
      reasoning: ["low", "medium", "high"],
      defaultReasoning: "medium",
      inputModalities: ["text"],
    },
  ],
  authorityProfiles: ["readOnly", "workspace"],
  authorityCeiling: "workspace",
  themes: themeCatalog,
  selectedThemeId: "tidepool",
  devices: devicesCatalog,
  capabilities: {
    attachments: true,
    documents: true,
    trustedProjectFiles: true,
    projectFileBrowser: true,
    projectFileWrite: true,
    transcriptSearch: true,
    attachmentPolicy: {
      acceptedMediaTypes: ["image/*", "text/*", "application/pdf"],
      maxCount: 8,
      maxFileBytes: 5_242_880,
      maxTotalBytes: 20_971_520,
    },
    previews: true,
    resources: true,
    connectedDevices: true,
    lanClients: true,
    terminal: false,
    attachmentIngest: true,
    pairDevices: true,
    sessionMetadata: true,
    sessionBranches: true,
    conversationBranching: true,
    sessionTrash: true,
    sessionExport: false,
    themeSelection: true,
    steer: true,
    followUp: true,
  },
};

export const fixtureSessions: Record<string, SessionSnapshot> = {
  "session-fresh": {
    sessionId: "session-fresh",
    actorGeneration: 1,
    sequence: 1,
    title: "New session",
    status: "idle",
    projectId: "project-ygg",
    modelId: "claude-sonnet-4-6",
    reasoning: "high",
    authority: "workspace",
    context: contextUsage(0),
    contextTokens: 0,
    contextPercent: 0,
    startedAt: at(58),
    branches: { entries: [], truncated: false },
    items: [],
    progress: [],
    sources: [],
    outputs: [],
    previews: [],
  },
  "session-live": {
    sessionId: "session-live",
    actorGeneration: 3,
    sequence: 28,
    title: "Refine onboarding preview",
    status: "working",
    activeRunId: "run-live",
    projectId: "project-ygg",
    modelId: "gpt-5.4",
    reasoning: "high",
    authority: "workspace",
    context: contextUsage(72_000),
    contextTokens: 72_000,
    contextPercent: 36,
    startedAt: at(42),
    branches: { entries: [], truncated: false },
    items: [
      {
        id: "live-user",
        turnId: "live-turn",
        kind: "user_message",
        content:
          "Tighten the onboarding flow and make sure the preview feels good on a phone.",
        state: "committed",
        createdAt: at(42),
      },
      {
        id: "live-assistant-intro",
        turnId: "live-turn",
        kind: "assistant_message",
        content:
          "I’ll trace the current onboarding states, adjust the responsive composition, and verify the result in the browser.",
        state: "committed",
        createdAt: at(43),
      },
      {
        id: "live-read",
        runId: "run-live",
        turnId: "live-turn",
        kind: "action",
        actionKind: "file_read",
        phase: "investigated",
        status: "succeeded",
        rawToolName: "read",
        label: "Read onboarding flow",
        target: "apps/web/src/onboarding",
        detail: "Read 7 files",
        state: "committed",
        createdAt: at(44),
        durationMs: 1280,
        observedOutputBytes: 8_240,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: ["source-onboarding", "source-tokens"],
        outputIds: [],
      },
      {
        id: "live-edit",
        runId: "run-live",
        turnId: "live-turn",
        kind: "action",
        actionKind: "file_write",
        phase: "changed",
        status: "succeeded",
        rawToolName: "apply_patch",
        label: "Refined responsive layout",
        target: "OnboardingFlow.tsx",
        detail: "Unified the narrow and desktop navigation states.",
        state: "committed",
        createdAt: at(48),
        durationMs: 3230,
        observedOutputBytes: 412,
        droppedOutputBytes: 0,
        changedPaths: ["apps/web/src/OnboardingFlow.tsx"],
        sourceIds: [],
        outputIds: [],
        additions: 84,
        deletions: 31,
      },
      {
        id: "live-preview",
        runId: "run-live",
        turnId: "live-turn",
        kind: "action",
        actionKind: "preview",
        phase: "verified",
        status: "succeeded",
        rawToolName: "browser",
        label: "Preview is live",
        target: "localhost:5173",
        detail: "Desktop and phone views connected.",
        state: "committed",
        createdAt: at(51),
        observedOutputBytes: 0,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: ["output-onboarding"],
      },
      {
        id: "live-reasoning",
        runId: "run-live",
        turnId: "live-turn",
        kind: "reasoning",
        summary: "Checking the narrow layout",
        content:
          "The mobile composition now keeps the primary action visible without flattening the hierarchy. I’m checking focus order and the final 393 px state.",
        state: "streaming",
        createdAt: at(54),
      },
    ],
    progress: [
      {
        id: "progress-1",
        content: "Map the current onboarding states",
        activeForm: "Mapping onboarding states",
        status: "completed",
      },
      {
        id: "progress-2",
        content: "Refine desktop and mobile composition",
        activeForm: "Refining responsive composition",
        status: "completed",
      },
      {
        id: "progress-3",
        content: "Verify keyboard, touch, and focus behavior",
        activeForm: "Verifying keyboard and touch behavior",
        status: "in_progress",
      },
    ],
    sources: [
      {
        id: "source-onboarding",
        kind: "file",
        title: "OnboardingFlow.tsx",
        subtitle: "Consulted · 2 minutes ago",
        consultedAt: at(44),
        iconLabel: "TSX",
        excerpt: "The current multi-step onboarding composition.",
      },
      {
        id: "source-tokens",
        kind: "file",
        title: "tokens.css",
        subtitle: "Consulted · 2 minutes ago",
        consultedAt: at(44),
        iconLabel: "CSS",
        excerpt: "Semantic spacing, color, and typography tokens.",
      },
    ],
    outputs: [
      {
        id: "output-onboarding",
        kind: "site",
        title: "Onboarding preview",
        subtitle: "Live local preview",
        mimeType: "text/html",
        updatedAt: at(53),
        previewId: "preview-onboarding",
      },
    ],
    previews: [
      {
        id: "preview-onboarding",
        title: "Onboarding preview",
        kind: "web",
        status: "live",
        urlLabel: "localhost:5173/onboarding",
        fixtureId: "onboarding",
        outputId: "output-onboarding",
      },
    ],
  },
  "session-attention": {
    sessionId: "session-attention",
    actorGeneration: 2,
    sequence: 16,
    title: "Prepare signed macOS build",
    status: "needs_attention",
    projectId: "project-ygg",
    modelId: "claude-sonnet-4-6",
    reasoning: "medium",
    authority: "readOnly",
    context: contextUsage(42_000),
    contextTokens: 42_000,
    contextPercent: 21,
    startedAt: at(35),
    branches: { entries: [], truncated: false },
    items: [
      {
        id: "attention-user",
        turnId: "attention-turn",
        kind: "user_message",
        content:
          "Prepare a signed macOS build and tell me exactly what is still missing.",
        state: "committed",
        createdAt: at(35),
      },
      {
        id: "attention-assistant",
        turnId: "attention-turn",
        kind: "assistant_message",
        content:
          "The release build is ready. I need permission to read the selected signing identity before I can sign it.",
        state: "committed",
        createdAt: at(39),
      },
      {
        id: "attention-action",
        runId: "run-attention",
        turnId: "attention-turn",
        kind: "action",
        actionKind: "command",
        phase: "produced",
        status: "succeeded",
        rawToolName: "bash",
        label: "Built release application",
        target: "target/release/ygg.app",
        commandPreview: "cargo build --release",
        detail: "Build completed without warnings.",
        state: "committed",
        createdAt: at(41),
        durationMs: 64_420,
        observedOutputBytes: 12_644,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: ["output-build"],
      },
      {
        id: "attention-approval",
        turnId: "attention-turn",
        kind: "approval",
        requestId: "approval-keychain",
        title: "Allow signing identity access?",
        description:
          "ygg wants to use “Developer ID Application” from this Mac’s Keychain to sign the build.",
        scopeLabel: "This signing step only",
        state: "streaming",
        createdAt: at(48),
      },
    ],
    progress: [
      {
        id: "attention-progress-1",
        content: "Create release build",
        activeForm: "Creating release build",
        status: "completed",
      },
      {
        id: "attention-progress-2",
        content: "Sign application",
        activeForm: "Signing application",
        status: "in_progress",
      },
      {
        id: "attention-progress-3",
        content: "Verify signature",
        activeForm: "Verifying signature",
        status: "pending",
      },
    ],
    sources: [],
    outputs: [
      {
        id: "output-build",
        kind: "file",
        title: "ygg.app",
        subtitle: "Unsigned release build · 42 MB",
        mimeType: "application/x-macos-app",
        updatedAt: at(42),
      },
    ],
    previews: [],
  },
  "session-done": {
    sessionId: "session-done",
    actorGeneration: 1,
    sequence: 41,
    title: "Review release readiness",
    status: "done",
    projectId: "project-ygg",
    modelId: "qwen3.5-27b",
    reasoning: "high",
    authority: "workspace",
    context: contextUsage(96_000),
    contextTokens: 96_000,
    contextPercent: 48,
    startedAt: at(8),
    branches: {
      head: "entry-release-ready",
      entries: [
        {
          entryId: "entry-release-question",
          kind: "userMessage",
          checkoutable: true,
          label: "Review release readiness",
        },
        {
          entryId: "entry-release-draft",
          parentEntryId: "entry-release-question",
          kind: "assistantMessage",
          checkoutable: true,
          label: "Initial release assessment",
        },
        {
          entryId: "entry-release-ready",
          parentEntryId: "entry-release-question",
          kind: "assistantMessage",
          checkoutable: true,
          label: "Verified release assessment",
        },
      ],
      truncated: false,
    },
    items: [
      {
        id: "done-user",
        turnId: "done-turn",
        kind: "user_message",
        content:
          "Review the release candidate, run the important checks, and give me something visual I can inspect.",
        state: "committed",
        createdAt: at(8),
      },
      {
        id: "done-intro",
        turnId: "done-turn",
        kind: "assistant_message",
        content:
          "I’ll verify the release candidate against the focused gates, inspect the changed files, and build a compact review artifact.",
        state: "committed",
        createdAt: at(9),
      },
      {
        id: "done-command",
        runId: "run-done",
        turnId: "done-turn",
        kind: "action",
        actionKind: "command",
        phase: "verified",
        status: "succeeded",
        rawToolName: "bash",
        label: "Ran focused release checks",
        target: "cargo test --workspace",
        commandPreview: "cargo test --workspace",
        detail: "42 checks passed · 0 failed",
        state: "committed",
        createdAt: at(18),
        durationMs: 83_240,
        exitCode: 0,
        observedOutputBytes: 24_804,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: ["source-cargo", "source-changelog"],
        outputIds: [],
      },
      {
        id: "done-report",
        runId: "run-done",
        turnId: "done-turn",
        kind: "action",
        actionKind: "file_write",
        phase: "produced",
        status: "succeeded",
        rawToolName: "write",
        label: "Created release pulse",
        target: "release-pulse.html",
        detail: "Interactive summary ready to inspect.",
        state: "committed",
        createdAt: at(27),
        observedOutputBytes: 312,
        droppedOutputBytes: 0,
        changedPaths: ["release-pulse.html"],
        sourceIds: [],
        additions: 184,
        deletions: 0,
        outputIds: ["output-release-pulse"],
      },
      {
        id: "done-answer",
        turnId: "done-turn",
        kind: "assistant_message",
        content:
          "The candidate is ready for review. All 42 focused checks passed, the eight changed files match the intended release scope, and I found no open blockers. I created a live release pulse with the evidence and key counts.",
        state: "committed",
        createdAt: at(30),
      },
      {
        id: "done-outcome",
        runId: "run-done",
        turnId: "done-turn",
        kind: "run_outcome",
        outcome: "done",
        durationMs: 132_000,
        summary: "Release review completed",
        review: {
          summary:
            "All focused checks passed and the review artifact is ready.",
          durationMs: 132_000,
          actionCount: 2,
          phases: [
            {
              phase: "verified",
              actionCount: 1,
              succeededCount: 1,
              failedCount: 0,
              stoppedCount: 0,
            },
            {
              phase: "produced",
              actionCount: 1,
              succeededCount: 1,
              failedCount: 0,
              stoppedCount: 0,
            },
          ],
          changedFileItemIds: ["done-report"],
          verificationActionItemIds: ["done-command"],
          failedActionItemIds: [],
          warningActionItemIds: [],
          sourceIds: ["source-cargo", "source-changelog"],
          outputIds: ["output-release-pulse"],
          testResults: [],
          evidenceCoverage: "complete",
          openQuestions: [],
        },
        state: "committed",
        createdAt: at(31),
      },
    ],
    progress: [
      {
        id: "done-progress-1",
        content: "Inspect release scope",
        activeForm: "Inspecting release scope",
        status: "completed",
      },
      {
        id: "done-progress-2",
        content: "Run focused checks",
        activeForm: "Running focused checks",
        status: "completed",
      },
      {
        id: "done-progress-3",
        content: "Create review artifact",
        activeForm: "Creating review artifact",
        status: "completed",
      },
    ],
    sources: [
      {
        id: "source-cargo",
        kind: "file",
        title: "Cargo.toml",
        subtitle: "Consulted · 15 minutes ago",
        consultedAt: at(14),
        iconLabel: "TOML",
        excerpt: "Workspace release profile and package inventory.",
      },
      {
        id: "source-changelog",
        kind: "file",
        title: "CHANGELOG.md",
        subtitle: "Consulted · 14 minutes ago",
        consultedAt: at(15),
        iconLabel: "MD",
        excerpt: "Release notes and compatibility changes.",
      },
      {
        id: "source-security",
        kind: "documentation",
        title: "SECURITY.md",
        subtitle: "Consulted · 12 minutes ago",
        consultedAt: at(17),
        iconLabel: "DOC",
        excerpt: "Release security policy and disclosure boundary.",
      },
    ],
    outputs: [
      {
        id: "output-release-pulse",
        kind: "site",
        title: "Release pulse",
        subtitle: "Interactive HTML · updated just now",
        mimeType: "text/html",
        updatedAt: at(29),
        previewId: "preview-release-pulse",
      },
      {
        id: "output-release-notes",
        kind: "document",
        title: "Release notes",
        subtitle: "Markdown · 3.4 KB",
        mimeType: "text/markdown",
        updatedAt: at(29),
        content:
          "# Release notes\n\n- All focused checks pass.\n- Eight intended files changed.\n- No release blockers remain.",
      },
    ],
    previews: [
      {
        id: "preview-release-pulse",
        title: "Release pulse",
        kind: "web",
        status: "live",
        urlLabel: "ygg.local/release-pulse",
        fixtureId: "release-pulse",
        outputId: "output-release-pulse",
      },
    ],
  },
  "session-recent": {
    sessionId: "session-recent",
    actorGeneration: 1,
    sequence: 9,
    title: "Summarize provider notes",
    status: "idle",
    projectId: "project-notes",
    modelId: "qwen3.5-27b",
    reasoning: "medium",
    authority: "readOnly",
    context: contextUsage(36_000),
    contextTokens: 36_000,
    contextPercent: 18,
    startedAt: at(2),
    branches: { entries: [], truncated: false },
    items: [
      {
        id: "recent-user",
        turnId: "recent-turn",
        kind: "user_message",
        content: "Summarize the provider notes from this folder.",
        state: "committed",
        createdAt: at(2),
      },
      {
        id: "recent-answer",
        turnId: "recent-turn",
        kind: "assistant_message",
        content:
          "The local endpoints differ mainly in cold-start behavior, context limits, and tool-call reliability. The notes favor the Qwen endpoint for everyday work and the larger reasoning model for architecture reviews.",
        state: "committed",
        createdAt: at(6),
      },
    ],
    progress: [],
    sources: [],
    outputs: [],
    previews: [],
  },
};

function createPerformanceSession(): SessionSnapshot {
  const items: SessionSnapshot["items"] = [];
  const longCodeBlock = [
    "```ts",
    ...Array.from(
      { length: 120 },
      (_, index) =>
        `const result${index} = await verifyShard(${index}, { retries: 2 });`,
    ),
    "```",
  ].join("\n");

  const shellRunId = "performance-shell-run";
  const shellTurnId = "performance-shell-turn";
  const shellCreatedAt = new Date(
    Date.UTC(2026, 6, 26, 11, 58, 0),
  ).toISOString();
  items.push(
    {
      id: "performance-shell-user",
      runId: shellRunId,
      turnId: shellTurnId,
      kind: "user_message",
      content:
        "Run every verification shard and keep the command history compact.",
      state: "committed",
      createdAt: shellCreatedAt,
    },
    {
      id: "performance-shell-intro",
      runId: shellRunId,
      turnId: shellTurnId,
      kind: "assistant_message",
      content:
        "I’ll run the full shard matrix, preserve each exit result, and summarize it as one verification phase.",
      state: "committed",
      createdAt: shellCreatedAt,
    },
  );
  for (let command = 0; command < 100; command += 1) {
    items.push({
      id: `performance-shell-command-${command}`,
      runId: shellRunId,
      turnId: shellTurnId,
      kind: "action",
      actionKind: "command",
      phase: "verified",
      status: "succeeded",
      rawToolName: "bash",
      label: `Ran shell check ${command + 1}`,
      target: `npm test -- --run shard-${command + 1}`,
      commandPreview: `npm test -- --run shard-${command + 1}`,
      detail: `Shard ${command + 1} passed with 24 assertions.`,
      state: "committed",
      createdAt: shellCreatedAt,
      durationMs: 420 + command,
      exitCode: 0,
      observedOutputBytes: 2_048 + command,
      droppedOutputBytes: 0,
      changedPaths: [],
      sourceIds: [],
      outputIds: [],
    });
  }
  items.push(
    {
      id: "performance-shell-reasoning",
      runId: shellRunId,
      turnId: shellTurnId,
      kind: "reasoning",
      summary: "Reconciled the shard matrix",
      content:
        "All command exits are accounted for without expanding 100 repetitive cards by default.",
      state: "committed",
      createdAt: shellCreatedAt,
    },
    {
      id: "performance-shell-answer",
      runId: shellRunId,
      turnId: shellTurnId,
      kind: "assistant_message",
      content: `The 100-shard matrix passed.\n\n${longCodeBlock}`,
      state: "committed",
      createdAt: shellCreatedAt,
    },
  );

  for (let turn = 0; turn < 224; turn += 1) {
    const turnId = `performance-turn-${turn}`;
    const createdAt = new Date(
      Date.UTC(2026, 6, 26, 12, 0, turn),
    ).toISOString();
    items.push(
      {
        id: `${turnId}-user`,
        turnId,
        kind: "user_message",
        content: `Review performance batch ${turn + 1}.`,
        state: "committed",
        createdAt,
      },
      {
        id: `${turnId}-intro`,
        turnId,
        kind: "assistant_message",
        content: `I’ll inspect batch ${turn + 1} and keep the evidence concise.`,
        state: "committed",
        createdAt,
      },
      {
        id: `${turnId}-reasoning`,
        turnId,
        kind: "reasoning",
        summary: `Checked batch ${turn + 1}`,
        content: "The batch is consistent with the expected output.",
        state: "committed",
        createdAt,
      },
      {
        id: `${turnId}-answer`,
        turnId,
        kind: "assistant_message",
        content:
          turn % 25 === 0
            ? `Batch ${turn + 1} is verified.\n\n${longCodeBlock}`
            : `Batch ${turn + 1} is verified with no unresolved failures.`,
        state: turn === 223 ? "streaming" : "committed",
        createdAt,
      },
    );
  }

  return {
    sessionId: "session-performance",
    actorGeneration: 1,
    sequence: 1_001,
    title: "Profile 1,000-item transcript",
    status: "working",
    activeRunId: "performance-run",
    projectId: "project-ygg",
    modelId: "gpt-5.4",
    reasoning: "high",
    authority: "workspace",
    context: contextUsage(164_000),
    contextTokens: 164_000,
    contextPercent: 82,
    startedAt: new Date(Date.UTC(2026, 6, 26, 12, 0, 0)).toISOString(),
    branches: { entries: [], truncated: false },
    items,
    progress: [
      {
        id: "performance-progress",
        content: "Stream final verification",
        activeForm: "Streaming final verification",
        status: "in_progress",
      },
    ],
    sources: [],
    outputs: [],
    previews: [],
  };
}

function createPerformanceReplaySession(): SessionSnapshot {
  const startedAt = new Date(
    Date.UTC(2026, 6, 26, 12, 4, 0),
  ).toISOString();
  return {
    sessionId: "session-performance-replay",
    actorGeneration: 4,
    sequence: 3_407,
    title: "Recovered replay after reconnect",
    status: "working",
    activeRunId: "performance-replay-run",
    projectId: "project-ygg",
    modelId: "claude-sonnet-4-6",
    reasoning: "high",
    authority: "workspace",
    context: contextUsage(94_000),
    contextTokens: 94_000,
    contextPercent: 47,
    startedAt,
    branches: { entries: [], truncated: false },
    items: [
      {
        id: "performance-replay-user",
        turnId: "performance-replay-turn",
        kind: "user_message",
        content:
          "Reconnect this background run and recover every durable event after the last cursor.",
        state: "committed",
        createdAt: startedAt,
      },
      {
        id: "performance-replay-action",
        runId: "performance-replay-run",
        turnId: "performance-replay-turn",
        kind: "action",
        actionKind: "analysis",
        phase: "investigated",
        status: "succeeded",
        rawToolName: "session_replay",
        label: "Replayed durable session events",
        summary: "Recovered the projection from cursor 3,392 through 3,406.",
        detail:
          "Actor generation 4 resumed without duplicating transcript items.",
        state: "committed",
        createdAt: startedAt,
        durationMs: 184,
        observedOutputBytes: 4_128,
        droppedOutputBytes: 0,
        changedPaths: [],
        sourceIds: [],
        outputIds: [],
      },
      {
        id: "performance-replay-assistant",
        runId: "performance-replay-run",
        turnId: "performance-replay-turn",
        kind: "assistant_message",
        content:
          "Replay is current through sequence 3,407. The background verification is continuing from the recovered projection.",
        state: "streaming",
        createdAt: startedAt,
      },
    ],
    progress: [
      {
        id: "performance-replay-progress",
        content: "Continue recovered verification",
        activeForm: "Continuing recovered verification",
        status: "in_progress",
      },
    ],
    sources: [],
    outputs: [],
    previews: [],
  };
}

if (
  import.meta.env.DEV &&
  typeof window !== "undefined" &&
  new URLSearchParams(window.location.search).get("fixture") === "performance"
) {
  const performanceSession = createPerformanceSession();
  const replaySession = createPerformanceReplaySession();
  fixtureSessions[performanceSession.sessionId] = performanceSession;
  fixtureSessions[replaySession.sessionId] = replaySession;
  fixtureBootstrap.selectedSessionId = performanceSession.sessionId;
  fixtureBootstrap.sessions.unshift(
    {
      id: performanceSession.sessionId,
      projectId: performanceSession.projectId,
      title: performanceSession.title,
      preview: "1,000 items · 100 shell calls · long Markdown",
      status: performanceSession.status,
      updatedAt: performanceSession.startedAt,
      pinned: true,
      archived: false,
      lifecycle: "active",
      unread: false,
      modelId: performanceSession.modelId,
      attentionCount: 0,
    },
    {
      id: replaySession.sessionId,
      projectId: replaySession.projectId,
      title: replaySession.title,
      preview: "Actor 4 · replayed through sequence 3,407",
      status: replaySession.status,
      updatedAt: replaySession.startedAt,
      pinned: true,
      archived: false,
      lifecycle: "active",
      unread: true,
      modelId: replaySession.modelId,
      attentionCount: 0,
    },
  );
  const project = fixtureBootstrap.projects.find(
    (candidate) => candidate.id === performanceSession.projectId,
  );
  if (project) {
    project.sessionCount += 2;
    project.liveSessionCount += 2;
  }
}

export const fixtureDevices = devicesCatalog;

export function getFixturePreviewMarkup(fixtureId: string): string | undefined {
  return fixtureId === "onboarding" || fixtureId === "release-pulse"
    ? previewMarkup
    : undefined;
}
