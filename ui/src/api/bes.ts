import axios from "axios";
import type {
  BuildPage,
  Page,
  BuildSummary,
  GlobalStats,
  BuildEvent,
  BuildInputs,
  BuildProfile,
  FileReference,
  Ecosystem,
  ExternalsPage,
  TargetCacheMiss,
  TargetSummary,
  TargetExecution,
  TestSummary,
  TestExecution,
} from "../types";
import { authHeader, keycloak } from "../auth/keycloak";

const api = axios.create({
  baseURL: "/api",
  headers: {
    "Content-Type": "application/json",
  },
});

api.interceptors.request.use(async (config) => {
  const headers = await authHeader();
  if (headers.Authorization) {
    config.headers.Authorization = headers.Authorization;
  }
  return config;
});

api.interceptors.response.use(undefined, (error) => {
  if (axios.isAxiosError(error) && error.response?.status === 401) {
    keycloak?.login();
  }
  return Promise.reject(error);
});

export async function getStats(): Promise<GlobalStats> {
  const { data } = await api.get<GlobalStats>("/stats");
  return data;
}

export interface BuildsQuery {
  limit?: number;
  cursor?: string;
  branch?: string;
  pr?: string;
}

export async function getBuilds(query: BuildsQuery = {}): Promise<BuildPage> {
  const { data } = await api.get<BuildPage>("/builds", { params: query });
  return data;
}

export async function getBuild(id: string): Promise<BuildSummary> {
  const { data } = await api.get<BuildSummary>(
    `/builds/${encodeURIComponent(id)}`,
  );
  return data;
}

export async function getEvents(
  id: string,
  important = true,
): Promise<BuildEvent[]> {
  const { data } = await api.get<BuildEvent[]>(
    `/builds/${encodeURIComponent(id)}/events`,
    { params: important ? { important: true } : {} },
  );
  return data;
}

/** The build's profile, or null when Bazel uploaded none or the CAS evicted it. */
export async function getProfile(id: string): Promise<BuildProfile | null> {
  try {
    const { data } = await api.get<BuildProfile>(
      `/builds/${encodeURIComponent(id)}/profile`,
    );
    return data;
  } catch (e) {
    if (axios.isAxiosError(e) && [404, 410].includes(e.response?.status ?? 0))
      return null;
    throw e;
  }
}

export async function getInputs(id: string): Promise<BuildInputs> {
  const { data } = await api.get<BuildInputs>(
    `/builds/${encodeURIComponent(id)}/inputs`,
  );
  return data;
}

export async function getLog(id: string): Promise<string> {
  const { data } = await api.get<string>(
    `/builds/${encodeURIComponent(id)}/log`,
    { responseType: "text", transformResponse: (r) => r },
  );
  return data;
}

/** Where the UI can read a blob Bazel uploaded to the CAS (action logs). */
export function blobUrl(ref?: FileReference | null) {
  if (!ref?.digest || !ref.length) return undefined;
  return `/api/blobs/${ref.digest}/${ref.length}`;
}

export async function getBlobText(url: string): Promise<string> {
  const headers = await authHeader();
  const { data } = await axios.get<string>(url, {
    headers,
    responseType: "text",
    transformResponse: (r) => r,
  });
  return data;
}

export async function getMisses(id: string): Promise<TargetCacheMiss[]> {
  const { data } = await api.get<TargetCacheMiss[]>(
    `/builds/${encodeURIComponent(id)}/misses`,
  );
  return data;
}

export async function getBuildTargets(id: string): Promise<TargetExecution[]> {
  const { data } = await api.get<TargetExecution[]>(
    `/builds/${encodeURIComponent(id)}/targets`,
  );
  return data;
}

export interface LabelsQuery {
  limit?: number;
  offset?: number;
  q?: string;
  status?: string;
}

export async function getTargets(
  query: LabelsQuery = {},
): Promise<Page<TargetSummary>> {
  const { data } = await api.get<Page<TargetSummary>>("/targets", {
    params: query,
  });
  return data;
}

export async function getTargetHistory(
  label: string,
): Promise<TargetExecution[]> {
  const { data } = await api.get<TargetExecution[]>(
    `/targets/${encodeURIComponent(label)}`,
  );
  return data;
}

export async function getTests(
  query: LabelsQuery = {},
): Promise<Page<TestSummary>> {
  const { data } = await api.get<Page<TestSummary>>("/tests", {
    params: query,
  });
  return data;
}

export async function getTestHistory(label: string): Promise<TestExecution[]> {
  const { data } = await api.get<TestExecution[]>(
    `/tests/${encodeURIComponent(label)}`,
  );
  return data;
}

export interface ExternalsQuery {
  limit?: number;
  offset?: number;
  q?: string;
  ecosystem?: Ecosystem;
  sort?: "downloads" | "size" | "newest" | "name";
}

export async function getExternals(
  query: ExternalsQuery = {},
): Promise<ExternalsPage> {
  const { data } = await api.get<ExternalsPage>("/proxy/externals", {
    params: query,
  });
  return data;
}

export default api;
