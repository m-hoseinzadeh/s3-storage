// Typed client for the admin JSON API. Cookies carry the session, so every
// request uses `credentials: "include"`. The base path tracks Vite's BASE_URL
// (`/`, the panel's dedicated port) so the API root is `/api`.

const API = `${import.meta.env.BASE_URL}api`;

export class ApiError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${API}${path}`, {
    credentials: "include",
    ...init,
  });
  const ct = res.headers.get("content-type") ?? "";
  const isJson = ct.includes("application/json");
  if (!res.ok) {
    if (isJson) {
      const body = await res.json().catch(() => null);
      const err = body?.error;
      throw new ApiError(res.status, err?.code ?? "Error", err?.message ?? res.statusText);
    }
    throw new ApiError(res.status, "Error", res.statusText);
  }
  return (isJson ? await res.json() : (undefined as T)) as T;
}

function jsonBody(data: unknown): RequestInit {
  return { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(data) };
}

const qs = (params: Record<string, string | number | undefined>) =>
  Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== "")
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`)
    .join("&");

// ---- types ----

export interface ServerConfig {
  access_key: string;
  public_buckets: string[];
  domains: string[];
  domain_map: string[];
  allowed_origins: string[];
  api_public_url?: string | null;
  admin_session_ttl_secs?: number;
  admin_path: string;
  version: string;
}

// Partial update; omitted fields are left unchanged. A blank `api_public_url`
// clears the stored value.
export interface SettingsUpdate {
  public_buckets?: string[];
  domains?: string[];
  domain_map?: string[];
  allowed_origins?: string[];
  api_public_url?: string;
  admin_session_ttl_secs?: number;
}

export interface Stats {
  bucket_count: number;
  object_count: number;
  total_size: number;
  public_bucket_count: number;
  buckets: { name: string; objects: number; size: number; public: boolean; creation_date?: string }[];
}

export interface BucketInfo {
  name: string;
  creation_date?: string;
  public: boolean;
}

export interface ObjectInfo {
  key: string;
  size: number;
  last_modified?: string;
  etag?: string;
}

export interface Listing {
  objects: ObjectInfo[];
  prefixes: string[];
  is_truncated: boolean;
  next_token?: string;
  key_count?: number;
}

export interface ObjectHead {
  content_type?: string;
  content_length?: number;
  last_modified?: string;
  etag?: string;
  cache_control?: string;
  content_disposition?: string;
  content_encoding?: string;
  content_language?: string;
  expires?: string;
  metadata?: Record<string, string>;
  checksums: Record<string, string | null>;
}

export interface ExtractResult {
  ok: boolean;
  extracted: string[];
  skipped: string[];
  extracted_count: number;
  skipped_count: number;
}

export interface UploadSession {
  upload_id: string;
  bucket?: string;
  key?: string;
  initiated?: number;
}

export interface PartInfo {
  part_number?: number;
  size?: number;
  etag?: string;
  last_modified?: string;
}

// ---- endpoints ----

export const api = {
  // auth
  login: (access_key: string, secret_key: string) =>
    call<{ ok: boolean; access_key: string }>("/login", jsonBody({ access_key, secret_key })),
  logout: () => call<{ ok: boolean }>("/logout", { method: "POST" }),
  session: () => call<{ authenticated: boolean; access_key: string }>("/session"),

  // server
  config: () => call<ServerConfig>("/config"),
  updateSettings: (data: SettingsUpdate) =>
    call<{ ok: boolean }>("/settings", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    }),
  stats: () => call<Stats>("/stats"),

  // buckets
  listBuckets: () => call<{ buckets: BucketInfo[] }>("/buckets"),
  createBucket: (name: string) => call<{ ok: boolean }>("/buckets", jsonBody({ name })),
  deleteBucket: (name: string) => call<{ ok: boolean }>(`/buckets/${encodeURIComponent(name)}`, { method: "DELETE" }),

  // objects
  listObjects: (bucket: string, prefix?: string, delimiter = "/", token?: string) =>
    call<Listing>(`/objects?${qs({ bucket, prefix, delimiter, token })}`),
  head: (bucket: string, key: string) => call<ObjectHead>(`/object/head?${qs({ bucket, key })}`),
  downloadUrl: (bucket: string, key: string) => `${API}/object/get?${qs({ bucket, key })}`,
  uploadUrl: (bucket: string, key: string, contentType?: string) =>
    `${API}/object/put?${qs({ bucket, key, content_type: contentType })}`,
  deleteObject: (bucket: string, key: string) =>
    call<{ ok: boolean }>(`/object?${qs({ bucket, key })}`, { method: "DELETE" }),
  batchDelete: (bucket: string, keys: string[]) =>
    call<{ ok: boolean; deleted: string[] }>("/objects/delete", jsonBody({ bucket, keys })),
  copy: (src_bucket: string, src_key: string, dst_bucket: string, dst_key: string) =>
    call<{ ok: boolean }>("/object/copy", jsonBody({ src_bucket, src_key, dst_bucket, dst_key })),
  move: (src_bucket: string, src_key: string, dst_bucket: string, dst_key: string) =>
    call<{ ok: boolean }>("/object/move", jsonBody({ src_bucket, src_key, dst_bucket, dst_key })),
  updateMetadata: (data: {
    bucket: string;
    key: string;
    content_type?: string;
    cache_control?: string;
    content_disposition?: string;
    content_encoding?: string;
    content_language?: string;
    expires?: string;
    metadata?: Record<string, string>;
  }) => call<{ ok: boolean }>("/object/metadata", jsonBody(data)),
  createFolder: (bucket: string, prefix: string) => call<{ ok: boolean }>("/folder", jsonBody({ bucket, prefix })),
  extract: (bucket: string, key: string, dest_prefix: string, overwrite: boolean) =>
    call<ExtractResult>("/object/extract", jsonBody({ bucket, key, dest_prefix, overwrite })),
  presign: (bucket: string, key: string, method: "GET" | "PUT", expires: number) =>
    call<{ url: string; expires_in: number; method: string }>(
      `/object/presign?${qs({ bucket, key, method, expires })}`,
    ),

  // multipart
  listMultipart: (bucket?: string) => call<{ uploads: UploadSession[] }>(`/multipart?${qs({ bucket: bucket ?? "" })}`),
  listParts: (bucket: string, key: string, upload_id: string) =>
    call<{ parts: PartInfo[] }>(`/multipart/parts?${qs({ bucket, key, upload_id })}`),
  abortMultipart: (bucket: string, key: string, upload_id: string) =>
    call<{ ok: boolean }>(`/multipart?${qs({ bucket, key, upload_id })}`, { method: "DELETE" }),
};

// Upload with progress via XHR (fetch lacks upload progress).
export function uploadFile(
  url: string,
  file: File | Blob,
  onProgress?: (pct: number) => void,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", url, true);
    xhr.withCredentials = true;
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable && onProgress) onProgress(Math.round((e.loaded / e.total) * 100));
    };
    xhr.onload = () => (xhr.status >= 200 && xhr.status < 300 ? resolve() : reject(new ApiError(xhr.status, "Upload", xhr.responseText || "upload failed")));
    xhr.onerror = () => reject(new ApiError(0, "Network", "network error during upload"));
    xhr.send(file);
  });
}
