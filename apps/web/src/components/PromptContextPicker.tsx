import {
  Check,
  FileSearch,
  FileText,
  LoaderCircle,
  Paperclip,
  Search,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type {
  DocumentReference,
  TrustedFileCatalog,
  TrustedFileEntry,
  TrustedFileRead,
  TrustedFileSearchResult,
} from "../protocol";

interface PromptContextPickerProps {
  documents: DocumentReference[];
  projectFiles: TrustedFileEntry[];
  documentsAvailable: boolean;
  projectFilesAvailable: boolean;
  onUploadDocument: (file: File) => Promise<DocumentReference>;
  onRemoveDocument: (documentId: string) => void;
  onToggleProjectFile: (file: TrustedFileEntry) => void;
  onListProjectFiles: () => Promise<TrustedFileCatalog>;
  onSearchProjectFiles: (
    query: string,
  ) => Promise<TrustedFileSearchResult>;
  onReadProjectFile: (entryId: string) => Promise<TrustedFileRead>;
}

const documentAccept =
  "text/plain,text/markdown,.md,.markdown,application/pdf";

export function PromptContextPicker({
  documents,
  projectFiles,
  documentsAvailable,
  projectFilesAvailable,
  onUploadDocument,
  onRemoveDocument,
  onToggleProjectFile,
  onListProjectFiles,
  onSearchProjectFiles,
  onReadProjectFile,
}: PromptContextPickerProps) {
  const [open, setOpen] = useState(false);
  const [catalog, setCatalog] = useState<TrustedFileCatalog | null>(null);
  const [searchResult, setSearchResult] =
    useState<TrustedFileSearchResult | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [preview, setPreview] = useState<TrustedFileRead | null>(null);
  const [error, setError] = useState<string | null>(null);
  const documentInputRef = useRef<HTMLInputElement>(null);
  const pickerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const catalogRequestedRef = useRef(false);
  const searchSequenceRef = useRef(0);
  const selectedIds = new Set(projectFiles.map((file) => file.id));
  const files = searchResult
    ? searchResult.hits.map((hit) => hit.entry)
    : (catalog?.files ?? []);

  useEffect(() => {
    if (!open) {
      catalogRequestedRef.current = false;
      return;
    }
    if (
      !projectFilesAvailable ||
      catalog ||
      catalogRequestedRef.current
    ) {
      return;
    }
    catalogRequestedRef.current = true;
    setLoading(true);
    setError(null);
    void onListProjectFiles()
      .then(setCatalog)
      .catch((cause: unknown) =>
        setError(
          cause instanceof Error
            ? cause.message
            : "Project files could not be loaded.",
        ),
      )
      .finally(() => setLoading(false));
  }, [
    catalog,
    loading,
    onListProjectFiles,
    open,
    projectFilesAvailable,
  ]);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !pickerRef.current?.contains(event.target)
      ) {
        searchSequenceRef.current += 1;
        setOpen(false);
      }
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      searchSequenceRef.current += 1;
      setOpen(false);
      triggerRef.current?.focus();
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  if (!documentsAvailable && !projectFilesAvailable) return null;

  const search = async () => {
    const value = query.trim();
    if (!value) {
      setSearchResult(null);
      return;
    }
    setLoading(true);
    setError(null);
    const sequence = ++searchSequenceRef.current;
    try {
      const result = await onSearchProjectFiles(value);
      if (sequence === searchSequenceRef.current) setSearchResult(result);
    } catch (cause) {
      if (sequence === searchSequenceRef.current) {
        setError(
          cause instanceof Error
            ? cause.message
            : "Project-file search failed.",
        );
      }
    } finally {
      if (sequence === searchSequenceRef.current) setLoading(false);
    }
  };

  const upload = async (file: File) => {
    setUploading(true);
    setError(null);
    try {
      await onUploadDocument(file);
    } catch (cause) {
      setError(
        cause instanceof Error ? cause.message : "Document upload failed.",
      );
    } finally {
      setUploading(false);
    }
  };

  return (
    <div className="prompt-context-picker" ref={pickerRef}>
      <button
        ref={triggerRef}
        className="composer-context-button"
        type="button"
        aria-expanded={open}
        aria-haspopup="dialog"
        onClick={() =>
          setOpen((current) => {
            if (current) searchSequenceRef.current += 1;
            return !current;
          })
        }
        title="Add document or project context"
      >
        <FileSearch aria-hidden="true" />
        <span>Context</span>
        {documents.length + projectFiles.length > 0 ? (
          <strong>{documents.length + projectFiles.length}</strong>
        ) : null}
      </button>
      {open ? (
        <div
          className="composer-context-panel"
          role="dialog"
          aria-label="Add prompt context"
        >
          <div className="context-panel-heading">
            <div>
              <strong>Prompt context</strong>
              <small>
                Selected content is inserted visibly as reference text.
              </small>
            </div>
            <button
              type="button"
              aria-label="Close prompt context"
              onClick={() => setOpen(false)}
            >
              <X aria-hidden="true" />
            </button>
          </div>

          {documentsAvailable ? (
            <section className="context-section">
              <div className="context-section-title">
                <span>
                  <Paperclip aria-hidden="true" />
                  Uploaded documents
                </span>
                <input
                  ref={documentInputRef}
                  type="file"
                  hidden
                  accept={documentAccept}
                  onChange={(event) => {
                    const file = event.target.files?.[0];
                    event.target.value = "";
                    if (file) void upload(file);
                  }}
                />
                <button
                  type="button"
                  disabled={uploading || documents.length >= 8}
                  onClick={() => documentInputRef.current?.click()}
                >
                  {uploading ? (
                    <LoaderCircle aria-hidden="true" />
                  ) : (
                    <FileText aria-hidden="true" />
                  )}
                  Upload
                </button>
              </div>
              {documents.length ? (
                <div className="context-selection-list">
                  {documents.map((document) => (
                    <div key={document.id}>
                      <span>
                        <strong>{document.displayName}</strong>
                        <small>
                          {document.mediaType === "application/pdf"
                            ? `PDF${document.pageCount ? ` · ${document.pageCount} pages` : ""}`
                            : document.mediaType === "text/markdown"
                              ? "Markdown"
                              : "Text"}
                        </small>
                      </span>
                      <button
                        type="button"
                        aria-label={`Remove ${document.displayName}`}
                        onClick={() => onRemoveDocument(document.id)}
                      >
                        <X aria-hidden="true" />
                      </button>
                    </div>
                  ))}
                </div>
              ) : (
                <p>Upload UTF-8 text, Markdown, or an ordinary PDF.</p>
              )}
            </section>
          ) : null}

          {projectFilesAvailable ? (
            <section className="context-section">
              <div className="context-section-title">
                <span>
                  <FileSearch aria-hidden="true" />
                  Trusted project files
                </span>
                {catalog ? (
                  <small>
                    {catalog.summary.indexedFiles.toLocaleString()} safe text
                    files
                  </small>
                ) : null}
              </div>
              <form
                className="context-file-search"
                onSubmit={(event) => {
                  event.preventDefault();
                  void search();
                }}
              >
                <Search aria-hidden="true" />
                <input
                  value={query}
                  onChange={(event) => {
                    setQuery(event.target.value);
                    if (!event.target.value) setSearchResult(null);
                  }}
                  placeholder="Search trusted files"
                  aria-label="Search trusted project files"
                />
                <button type="submit" disabled={loading || !query.trim()}>
                  Search
                </button>
              </form>
              {loading ? (
                <div className="context-loading">
                  <LoaderCircle aria-hidden="true" />
                  Reading the trusted index…
                </div>
              ) : (
                <div className="context-file-list">
                  {files.slice(0, 100).map((file) => {
                    const selected = selectedIds.has(file.id);
                    return (
                      <div key={file.id}>
                        <button
                          type="button"
                          className={selected ? "is-selected" : ""}
                          disabled={!selected && projectFiles.length >= 20}
                          onClick={() => onToggleProjectFile(file)}
                          aria-pressed={selected}
                        >
                          <span>
                            <strong>{file.relativePath}</strong>
                            <small>
                              {file.kind} · {file.byteLen.toLocaleString()} bytes
                            </small>
                          </span>
                          {selected ? <Check aria-hidden="true" /> : null}
                        </button>
                        <button
                          type="button"
                          aria-label={`Preview ${file.relativePath}`}
                          onClick={() => {
                            setError(null);
                            void onReadProjectFile(file.id)
                              .then(setPreview)
                              .catch((cause: unknown) =>
                                setError(
                                  cause instanceof Error
                                    ? cause.message
                                    : "File preview failed.",
                                ),
                              );
                          }}
                        >
                          Preview
                        </button>
                      </div>
                    );
                  })}
                </div>
              )}
              {preview ? (
                <div className="context-file-preview">
                  <div>
                    <strong>{preview.entry.relativePath}</strong>
                    <button
                      type="button"
                      aria-label="Close file preview"
                      onClick={() => setPreview(null)}
                    >
                      <X aria-hidden="true" />
                    </button>
                  </div>
                  <pre>{preview.text}</pre>
                </div>
              ) : null}
            </section>
          ) : null}
          {error ? <p className="context-error">{error}</p> : null}
        </div>
      ) : null}
    </div>
  );
}
