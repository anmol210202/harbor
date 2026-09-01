import { safeFetch } from "@/lib/safe-fetch";
import { authToken, currentAuthor } from "@/lib/theme-auth";
import { readLists, type CustomList } from "@/lib/custom-lists";
import { bakeDefaultPosters } from "./featured-posters";
import { HARBOR_API_BASE } from "@/lib/config/endpoints";

export { likeList, unlikeList } from "./list-likes";
export type { ListLike } from "./list-likes";

const BASE = `${HARBOR_API_BASE}/themes/api/social`;

export const MAX_FEATURED_LISTS = 6;
export const MAX_FEATURED_ITEMS = 24;

export type FeaturedItem = {
  id: string;
  name: string;
  poster: string;
  type: string;
};

export type FeaturedList = {
  id: string;
  name: string;
  description?: string;
  items: FeaturedItem[];
  coverImage?: string;
  bgImage?: string;
  bgMode?: string;
  likeCount?: number;
  liked?: boolean;
};

export type PickableList = {
  id: string;
  name: string;
  description?: string;
  items: FeaturedItem[];
  coverImage?: string;
  bgImage?: string;
  bgMode?: string;
};

export function toPickableList(list: CustomList): PickableList {
  return {
    id: list.id,
    name: list.name,
    description: list.description,
    coverImage: list.coverImage,
    bgImage: list.bgImage,
    bgMode: list.bgMode,
    items: list.items.slice(0, MAX_FEATURED_ITEMS).map((it) => ({
      id: it.id,
      name: it.name,
      poster: it.poster ?? "",
      type: it.type,
    })),
  };
}

export function readLocalLists(): PickableList[] {
  return readLists().map(toPickableList);
}

export function toFeaturedList(list: PickableList): FeaturedList {
  return {
    id: "",
    name: list.name,
    description: list.description,
    items: list.items,
    coverImage: list.coverImage,
    bgImage: list.bgImage,
    bgMode: list.bgMode,
  };
}

export function normalizeListName(name: string): string {
  return name.replace(/[<>]/g, "").replace(/\s+/g, " ").trim().slice(0, 40);
}

export function buildFeaturedPayload(
  selected: PickableList[],
  served: FeaturedList[],
): FeaturedList[] {
  const idByName = new Map<string, string>();
  const byName = new Map<string, FeaturedList>();
  for (const f of served) {
    const key = normalizeListName(f.name);
    if (f.id && !idByName.has(key)) idByName.set(key, f.id);
    if (!byName.has(key)) byName.set(key, f);
  }
  return selected.map((l) => {
    const key = normalizeListName(l.name);
    const existing = byName.get(key);
    return {
      id: idByName.get(key) ?? "",
      name: l.name,
      description: l.description ?? existing?.description,
      coverImage: l.coverImage,
      bgImage: l.bgImage,
      bgMode: l.bgMode,
      items: l.items,
    };
  });
}

function authHeaders(): Record<string, string> {
  const t = authToken();
  return t ? { authorization: `Bearer ${t}` } : {};
}

export function hydrateFeaturedListDescriptions(lists: FeaturedList[]): FeaturedList[] {
  const localByName = new Map(readLists().map((l) => [normalizeListName(l.name), l] as const));
  return lists.map((list) => {
    const local = localByName.get(normalizeListName(list.name));
    return {
      ...list,
      description: list.description ?? local?.description,
    };
  });
}

function readFeatured(data: unknown): FeaturedList[] {
  const lists = (data as { featuredLists?: unknown } | null)?.featuredLists;
  return Array.isArray(lists) ? hydrateFeaturedListDescriptions(lists as FeaturedList[]) : [];
}

export async function fetchFeaturedLists(
  handle: string,
  signal?: AbortSignal,
): Promise<FeaturedList[]> {
  const res = await safeFetch(`${BASE}/u/${encodeURIComponent(handle)}`, {
    headers: authHeaders(),
    signal,
  });
  if (!res.ok) throw new Error(`featured lists ${res.status}`);
  return readFeatured(await res.json());
}

export async function fetchSharedList(
  handle: string,
  listId: string,
  signal?: AbortSignal,
): Promise<FeaturedList | null> {
  const lists = await fetchFeaturedLists(handle, signal);
  return lists.find((l) => l.id === listId) ?? null;
}

export async function saveFeaturedLists(
  lists: FeaturedList[],
  clear = false,
): Promise<FeaturedList[]> {
  const baked = lists.length > 0 ? await bakeDefaultPosters(lists) : lists;
  const body: Record<string, unknown> = { featuredLists: baked };
  if (clear) body.clearFeaturedLists = true;
  const res = await safeFetch(`${BASE}/me/profile`, {
    method: "PATCH",
    headers: { ...authHeaders(), "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`save featured lists ${res.status}`);
  const echoed = readFeatured(await res.json());
  const merged = echoed.length ? hydrateFeaturedListDescriptions(echoed) : hydrateFeaturedListDescriptions(lists);
  return merged.length || lists.length === 0 ? merged : lists;
}

export async function unfeatureListByName(name: string): Promise<void> {
  const handle = currentAuthor()?.handle;
  const target = normalizeListName(name);
  if (!handle || !target) return;
  const served = await fetchFeaturedLists(handle);
  const kept = served.filter((l) => normalizeListName(l.name) !== target);
  if (kept.length !== served.length) await saveFeaturedLists(kept, true);
}

export function listShareUrl(handle: string, listId: string): string {
  return `${HARBOR_API_BASE}/list/${encodeURIComponent(handle)}/${encodeURIComponent(listId)}`;
}

export function listDeepLink(handle: string, listId: string): string {
  return `harbor://list/${encodeURIComponent(handle)}/${encodeURIComponent(listId)}`;
}
