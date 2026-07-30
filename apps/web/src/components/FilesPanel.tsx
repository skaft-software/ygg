import {
  ChevronDown,
  ChevronRight,
  FileCode2,
  Folder,
  FolderOpen,
  RefreshCw,
  Save,
  Search,
  TriangleAlert,
  X,
} from "lucide-react";
import {
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type {
  ProjectFileRead,
  ProjectFileSearchResult,
  ProjectFileTree,
  ProjectFileWrite,
  ProjectFileWriteRequest,
  ProjectSummary,
} from "../protocol";
import { ProjectFileConflictError } from "../transport";
import MarkdownMessage from "./MarkdownMessage";
import { FileCodeEditor } from "./FileCodeEditor";
import { isMarkdownPath, languageNameForPath } from "./fileLanguage";

interface DirectoryState {
  tree?: ProjectFileTree;
  loading: boolean;
  error: boolean;
}

type PendingNavigation =
  | { kind: "file"; path: string }
  | { kind: "project"; projectId: string };

export interface FilesPanelProps {
  projects: ProjectSummary[];
  preferredProjectId?: string;
  writeAvailable: boolean;
  getTree: (projectId: string, path?: string) => Promise<ProjectFileTree>;
  readFile: (
    projectId: string,
    path: string,
    startLine?: number,
    endLine?: number,
  ) => Promise<ProjectFileRead>;
  searchFiles: (
    projectId: string,
    query: string,
  ) => Promise<ProjectFileSearchResult>;
  writeFile: (
    projectId: string,
    request: ProjectFileWriteRequest,
  ) => Promise<ProjectFileWrite>;
}

function joinPath(parent: string, name: string): string {
  return parent ? `${parent}/${name}` : name;
}

function fileSize(size: number): string {
  if (size < 1_024) return `${size} B`;
  if (size < 1_024 * 1_024) return `${Math.ceil(size / 1_024)} KB`;
  return `${(size / (1_024 * 1_024)).toFixed(1)} MB`;
}

function selectableProjects(projects: ProjectSummary[]): ProjectSummary[] {
  return projects.filter(
    (project) => project.trusted && project.available && !project.archived,
  );
}

function defaultProjectId(
  projects: ProjectSummary[],
  preferredProjectId?: string,
): string {
  return (
    projects.find((project) => project.id === preferredProjectId)?.id ??
    projects[0]?.id ??
    ""
  );
}

interface ProjectFilesWorkspaceProps
  extends Omit<FilesPanelProps, "projects" | "preferredProjectId"> {
  availableProjects: ProjectSummary[];
  projectId: string;
  onSelectProject: (projectId: string) => void;
}

export function FilesPanel({
  projects,
  preferredProjectId,
  ...workspaceProps
}: FilesPanelProps) {
  const availableProjects = useMemo(() => selectableProjects(projects), [projects]);
  const [requestedProjectId, setRequestedProjectId] = useState(() =>
    defaultProjectId(availableProjects, preferredProjectId),
  );
  const projectId = availableProjects.some(
    (project) => project.id === requestedProjectId,
  )
    ? requestedProjectId
    : defaultProjectId(availableProjects, preferredProjectId);

  if (availableProjects.length === 0) {
    return (
      <main className="files-panel files-empty" aria-labelledby="files-title">
        <Folder aria-hidden="true" />
        <h1 id="files-title">Files</h1>
        <p>Trust and open a local project before browsing its files.</p>
      </main>
    );
  }

  return (
    <ProjectFilesWorkspace
      {...workspaceProps}
      key={projectId}
      availableProjects={availableProjects}
      projectId={projectId}
      onSelectProject={setRequestedProjectId}
    />
  );
}

function ProjectFilesWorkspace({
  availableProjects,
  projectId,
  onSelectProject,
  writeAvailable,
  getTree,
  readFile,
  searchFiles,
  writeFile,
}: ProjectFilesWorkspaceProps) {
  const [directories, setDirectories] = useState<Record<string, DirectoryState>>(
    () => ({ "": { loading: true, error: false } }),
  );
  const [expandedDirectories, setExpandedDirectories] = useState<Set<string>>(
    () => new Set(),
  );
  const [selectedFile, setSelectedFile] = useState<ProjectFileRead | null>(null);
  const [draft, setDraft] = useState("");
  const [markdownMode, setMarkdownMode] = useState<"preview" | "source">(
    "source",
  );
  const [fileLoading, setFileLoading] = useState(false);
  const [fileError, setFileError] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResult, setSearchResult] = useState<ProjectFileSearchResult | null>(
    null,
  );
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState(false);
  const [searchGeneration, setSearchGeneration] = useState(0);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [pendingNavigation, setPendingNavigation] =
    useState<PendingNavigation | null>(null);
  const directoryRequestsRef = useRef(new Map<string, number>());
  const fileRequestRef = useRef(0);

  const dirty = selectedFile !== null && draft !== selectedFile.content;
  const selectedPath = selectedFile?.path;
  const selectedProject = availableProjects.find(
    (project) => project.id === projectId,
  );
  const markdownFile = selectedFile !== null && isMarkdownPath(selectedFile.path);

  const loadDirectory = useCallback(
    async (path: string) => {
      if (!projectId) return;
      const requestedProject = projectId;
      const requestKey = `${requestedProject}\u0000${path}`;
      const request = (directoryRequestsRef.current.get(requestKey) ?? 0) + 1;
      directoryRequestsRef.current.set(requestKey, request);
      setDirectories((current) => ({
        ...current,
        [path]: { ...current[path], loading: true, error: false },
      }));
      try {
        const tree = await getTree(requestedProject, path);
        if (directoryRequestsRef.current.get(requestKey) !== request) {
          return;
        }
        setDirectories((current) => ({
          ...current,
          [path]: { tree, loading: false, error: false },
        }));
      } catch {
        if (directoryRequestsRef.current.get(requestKey) !== request) {
          return;
        }
        setDirectories((current) => ({
          ...current,
          [path]: { ...current[path], loading: false, error: true },
        }));
      }
    },
    [getTree, projectId],
  );

  const loadFile = useCallback(
    async (path: string) => {
      if (!projectId) return;
      const requestedProject = projectId;
      const request = ++fileRequestRef.current;
      setFileLoading(true);
      setFileError(false);
      setSaveError(false);
      setConflict(false);
      try {
        const file = await readFile(requestedProject, path);
        if (fileRequestRef.current !== request) {
          return;
        }
        setSelectedFile(file);
        setDraft(file.content);
        setMarkdownMode(isMarkdownPath(file.path) ? "preview" : "source");
      } catch {
        if (fileRequestRef.current === request) {
          setFileError(true);
        }
      } finally {
        if (fileRequestRef.current === request) {
          setFileLoading(false);
        }
      }
    },
    [projectId, readFile],
  );

  useEffect(() => {
    const path = "";
    const request = (directoryRequestsRef.current.get(path) ?? 0) + 1;
    directoryRequestsRef.current.set(path, request);
    let cancelled = false;
    void getTree(projectId, path)
      .then((tree) => {
        if (cancelled || directoryRequestsRef.current.get(path) !== request) return;
        setDirectories((current) => ({
          ...current,
          [path]: { tree, loading: false, error: false },
        }));
      })
      .catch(() => {
        if (cancelled || directoryRequestsRef.current.get(path) !== request) return;
        setDirectories((current) => ({
          ...current,
          [path]: { ...current[path], loading: false, error: true },
        }));
      });
    return () => {
      cancelled = true;
    };
  }, [getTree, projectId]);

  useEffect(() => {
    const query = searchQuery.trim();
    if (!query) return;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void searchFiles(projectId, query)
        .then((result) => {
          if (!cancelled) setSearchResult(result);
        })
        .catch(() => {
          if (!cancelled) setSearchError(true);
        })
        .finally(() => {
          if (!cancelled) setSearchLoading(false);
        });
    }, 180);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [projectId, searchFiles, searchGeneration, searchQuery]);

  const updateSearchQuery = (nextQuery: string) => {
    setSearchQuery(nextQuery);
    setSearchGeneration((generation) => generation + 1);
    setSearchResult(null);
    setSearchError(false);
    setSearchLoading(Boolean(nextQuery.trim()));
  };

  const openFile = useCallback(
    (path: string) => {
      if (path === selectedPath) return;
      if (dirty) {
        setPendingNavigation({ kind: "file", path });
        return;
      }
      void loadFile(path);
    },
    [dirty, loadFile, selectedPath],
  );

  const toggleDirectory = (path: string) => {
    const expanded = expandedDirectories.has(path);
    setExpandedDirectories((current) => {
      const next = new Set(current);
      if (expanded) next.delete(path);
      else next.add(path);
      return next;
    });
    if (!expanded && !directories[path]?.tree && !directories[path]?.loading) {
      void loadDirectory(path);
    }
  };

  const requestProject = (nextProjectId: string) => {
    if (nextProjectId === projectId) return;
    if (dirty) {
      setPendingNavigation({ kind: "project", projectId: nextProjectId });
      return;
    }
    onSelectProject(nextProjectId);
  };

  const discardPendingNavigation = () => {
    const pending = pendingNavigation;
    setPendingNavigation(null);
    setDraft(selectedFile?.content ?? "");
    setConflict(false);
    if (!pending) return;
    if (pending.kind === "file") void loadFile(pending.path);
    else onSelectProject(pending.projectId);
  };

  const save = useCallback(
    async (force = false) => {
      if (
        !projectId ||
        !selectedFile ||
        !selectedFile.sha256 ||
        selectedFile.truncated ||
        !writeAvailable ||
        saving
      ) {
        return;
      }
      const sourcePath = selectedFile.path;
      const sourceContent = draft;
      const sourceVersion = selectedFile.sha256;
      setSaving(true);
      setSaveError(false);
      if (!force) setConflict(false);
      try {
        const written = await writeFile(projectId, {
          path: sourcePath,
          content: sourceContent,
          expectedSha256: sourceVersion,
          force,
        });
        setSelectedFile((current) =>
          current?.path === sourcePath
            ? {
                ...current,
                path: written.path,
                content: sourceContent,
                sha256: written.sha256,
                modifiedAtMs: written.modifiedAtMs,
                truncated: false,
              }
            : current,
        );
        setConflict(false);
      } catch (error) {
        if (error instanceof ProjectFileConflictError) setConflict(true);
        else setSaveError(true);
      } finally {
        setSaving(false);
      }
    },
    [draft, projectId, saving, selectedFile, writeAvailable, writeFile],
  );

  useEffect(() => {
    if (!dirty || !writeAvailable) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() !== "s" || (!event.metaKey && !event.ctrlKey)) {
        return;
      }
      event.preventDefault();
      void save();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [dirty, save, writeAvailable]);

  const renderDirectory = (path: string, depth: number): ReactNode => {
    const directory = directories[path];
    if (directory?.loading && !directory.tree) {
      return (
        <p className="files-tree-status" role="status" key={`${path}-loading`}>
          Loading folder…
        </p>
      );
    }
    if (directory?.error && !directory.tree) {
      return (
        <div className="files-tree-status is-error" key={`${path}-error`}>
          <span>Folder could not be loaded.</span>
          <button type="button" onClick={() => void loadDirectory(path)}>
            Retry
          </button>
        </div>
      );
    }
    if (!directory?.tree) return null;
    return (
      <>
        {directory.tree.entries.map((entry) => {
          const entryPath = joinPath(path, entry.name);
          const directoryEntry = entry.kind === "directory";
          const expanded = expandedDirectories.has(entryPath);
          return (
            <div className="files-tree-entry" key={entryPath}>
              <button
                type="button"
                className={`files-tree-row ${
                  selectedPath === entryPath ? "is-selected" : ""
                }`}
                style={{ paddingInlineStart: 10 + depth * 16 }}
                aria-expanded={directoryEntry ? expanded : undefined}
                onClick={() =>
                  directoryEntry ? toggleDirectory(entryPath) : openFile(entryPath)
                }
                title={entryPath}
              >
                {directoryEntry ? (
                  expanded ? (
                    <ChevronDown className="files-tree-chevron" aria-hidden="true" />
                  ) : (
                    <ChevronRight className="files-tree-chevron" aria-hidden="true" />
                  )
                ) : (
                  <span className="files-tree-chevron" aria-hidden="true" />
                )}
                {directoryEntry ? (
                  expanded ? (
                    <FolderOpen aria-hidden="true" />
                  ) : (
                    <Folder aria-hidden="true" />
                  )
                ) : (
                  <FileCode2 aria-hidden="true" />
                )}
                <span>{entry.name}</span>
                {!directoryEntry ? <small>{fileSize(entry.size)}</small> : null}
              </button>
              {directoryEntry && expanded ? renderDirectory(entryPath, depth + 1) : null}
            </div>
          );
        })}
        {directory.tree.truncated ? (
          <p className="files-tree-status" role="status">
            This folder has more files than ygg can safely list.
          </p>
        ) : null}
      </>
    );
  };

  return (
    <main className="files-panel" aria-labelledby="files-title">
      <header className="files-panel-header">
        <div>
          <span>Trusted project browser</span>
          <h1 id="files-title">Files</h1>
        </div>
        <label className="files-project-picker">
          <span className="sr-only">Project</span>
          <select
            value={projectId}
            onChange={(event) => requestProject(event.target.value)}
          >
            {availableProjects.map((project) => (
              <option key={project.id} value={project.id}>
                {project.name}
              </option>
            ))}
          </select>
        </label>
      </header>

      {pendingNavigation ? (
        <div className="files-discard-prompt" role="alert">
          <TriangleAlert aria-hidden="true" />
          <p>You have unsaved changes.</p>
          <button type="button" className="secondary-button" onClick={() => setPendingNavigation(null)}>
            Keep editing
          </button>
          <button type="button" className="files-danger-button" onClick={discardPendingNavigation}>
            Discard changes
          </button>
        </div>
      ) : null}

      <div className="files-workbench">
        <aside className="files-browser" aria-label={`${selectedProject?.name ?? "Project"} files`}>
          <label className="files-search">
            <Search aria-hidden="true" />
            <span className="sr-only">Search project files</span>
            <input
              type="search"
              value={searchQuery}
              maxLength={256}
              onChange={(event) => updateSearchQuery(event.target.value)}
              placeholder="Search files"
            />
            {searchQuery ? (
              <button
                type="button"
                aria-label="Clear file search"
                onClick={() => updateSearchQuery("")}
              >
                <X aria-hidden="true" />
              </button>
            ) : null}
          </label>

          {searchQuery.trim() ? (
            <section className="files-search-results" aria-label="File search results">
              {searchLoading ? <p role="status">Searching files…</p> : null}
              {searchError ? (
                <p className="is-error" role="alert">
                  File search could not be completed.
                </p>
              ) : null}
              {!searchLoading && !searchError && searchResult?.hits.length === 0 ? (
                <p>No matching files.</p>
              ) : null}
              {searchResult?.hits.map((hit) => (
                <button
                  type="button"
                  key={`${hit.path}:${hit.line ?? 0}`}
                  onClick={() => openFile(hit.path)}
                >
                  <FileCode2 aria-hidden="true" />
                  <span>
                    <strong>{hit.path}</strong>
                    {hit.line ? <small>Line {hit.line}</small> : null}
                    {hit.snippet ? <em>{hit.snippet}</em> : null}
                  </span>
                </button>
              ))}
              {searchResult?.truncated ? (
                <p>This search reached a safe host limit.</p>
              ) : null}
            </section>
          ) : (
            <section className="files-tree" aria-label="Project file tree">
              {renderDirectory("", 0)}
            </section>
          )}
        </aside>

        <section className="files-editor" aria-label="File editor">
          {fileLoading ? (
            <div className="files-editor-state" role="status">
              <RefreshCw className="spin" aria-hidden="true" />
              Loading file…
            </div>
          ) : fileError ? (
            <div className="files-editor-state is-error" role="alert">
              <TriangleAlert aria-hidden="true" />
              <p>The selected file could not be read.</p>
              <button
                type="button"
                className="secondary-button"
                onClick={() => selectedPath && void loadFile(selectedPath)}
              >
                Retry
              </button>
            </div>
          ) : !selectedFile ? (
            <div className="files-editor-state">
              <FileCode2 aria-hidden="true" />
              <p>Select a file to inspect it.</p>
            </div>
          ) : (
            <>
              <header className="files-editor-header">
                <div>
                  <strong>{selectedFile.path}</strong>
                  <span>
                    {languageNameForPath(selectedFile.path)} · {selectedFile.lineCount}{" "}
                    {selectedFile.lineCount === 1 ? "line" : "lines"}
                    {selectedFile.truncated ? " · partial view" : ""}
                  </span>
                </div>
                <div className="files-editor-actions">
                  {dirty ? <span className="files-dirty">Unsaved</span> : null}
                  {markdownFile ? (
                    <button
                      type="button"
                      className="secondary-button"
                      aria-label={
                        markdownMode === "preview"
                          ? "Edit Markdown"
                          : "Preview Markdown"
                      }
                      onClick={() =>
                        setMarkdownMode((mode) =>
                          mode === "preview" ? "source" : "preview",
                        )
                      }
                    >
                      {markdownMode === "preview" ? "Edit Markdown" : "Preview Markdown"}
                    </button>
                  ) : null}
                  <button
                    type="button"
                    className="secondary-button"
                    disabled={
                      !dirty ||
                      !writeAvailable ||
                      !selectedFile.sha256 ||
                      selectedFile.truncated ||
                      saving
                    }
                    onClick={() => void save()}
                  >
                    <Save aria-hidden="true" />
                    {saving ? "Saving…" : "Save"}
                  </button>
                </div>
              </header>
              {selectedFile.truncated || !selectedFile.sha256 ? (
                <p className="files-editor-notice" role="status">
                  This is a partial file view and cannot be saved. Read a smaller file to edit it.
                </p>
              ) : !writeAvailable ? (
                <p className="files-editor-notice" role="status">
                  This host allows file browsing but its write policy is disabled.
                </p>
              ) : null}
              {saveError ? (
                <p className="files-editor-error" role="alert">
                  The file could not be saved. Your changes are still in the editor.
                </p>
              ) : null}
              {conflict ? (
                <div className="files-conflict" role="alert">
                  <TriangleAlert aria-hidden="true" />
                  <div>
                    <strong>This file changed on disk.</strong>
                    <p>Reload to review it, or explicitly overwrite it with your draft.</p>
                  </div>
                  <button
                    type="button"
                    className="secondary-button"
                    disabled={saving}
                    onClick={() => void loadFile(selectedFile.path)}
                  >
                    Reload file
                  </button>
                  <button
                    type="button"
                    className="files-danger-button"
                    disabled={saving}
                    onClick={() => void save(true)}
                  >
                    Overwrite
                  </button>
                </div>
              ) : null}
              {markdownFile && markdownMode === "preview" ? (
                <div
                  className="files-markdown-viewer"
                  aria-label="Rendered Markdown"
                >
                  <MarkdownMessage content={draft} />
                </div>
              ) : (
                <FileCodeEditor
                  path={selectedFile.path}
                  value={draft}
                  readOnly={!writeAvailable || selectedFile.truncated || !selectedFile.sha256}
                  onChange={(value) => {
                    setDraft(value);
                    setSaveError(false);
                    setConflict(false);
                  }}
                />
              )}
            </>
          )}
        </section>
      </div>
    </main>
  );
}
