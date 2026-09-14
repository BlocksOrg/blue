import type { CollectionEntry } from "astro:content";

export function newestFirst(posts: CollectionEntry<"blog">[]) {
  return posts.sort(
    (left, right) => right.data.publishDate.getTime() - left.data.publishDate.getTime(),
  );
}
