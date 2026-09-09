"use client";

import { useRouter } from "next/navigation";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const pageSizes = [25, 50, 75, 100];

export function PageSizeSelect({
  pathname,
  params,
  value,
  onChange,
}: {
  pathname: string;
  params: Record<string, string>;
  value: number;
  onChange?: (value: number) => void;
}) {
  const router = useRouter();

  function updatePageSize(next: string | null) {
    if (!next) return;
    if (onChange) {
      onChange(Number(next));
      return;
    }

    const search = new URLSearchParams(params);
    search.delete("page");
    if (next === "25") search.delete("per_page");
    else search.set("per_page", next);

    const query = search.toString();
    router.push(query ? `${pathname}?${query}` : pathname);
  }

  return (
    <label className="flex items-center gap-2 text-sm text-muted-foreground">
      Rows per page
      <Select value={String(value)} onValueChange={updatePageSize}>
        <SelectTrigger size="sm" className="w-18" aria-label="Rows per page">
          <SelectValue />
        </SelectTrigger>
        <SelectContent align="end">
          {pageSizes.map((size) => (
            <SelectItem key={size} value={String(size)}>
              {size}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </label>
  );
}
