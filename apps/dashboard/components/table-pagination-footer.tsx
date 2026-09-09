import Link from "next/link";
import {
  ChevronLeft,
  ChevronRight,
  ChevronsLeft,
  ChevronsRight,
} from "lucide-react";
import { PageSizeSelect } from "@/components/page-size-select";
import { Button, buttonVariants } from "@/components/ui/button";
import { cn } from "@/lib/utils";

function pageItems(current: number, total: number): (number | "ellipsis")[] {
  if (total <= 7) return Array.from({ length: total }, (_, index) => index + 1);
  if (current <= 4) return [1, 2, 3, 4, 5, "ellipsis", total];
  if (current >= total - 3)
    return [1, "ellipsis", total - 4, total - 3, total - 2, total - 1, total];
  return [1, "ellipsis", current - 1, current, current + 1, "ellipsis", total];
}

function hrefWith(
  pathname: string,
  current: Record<string, string>,
  changes: Record<string, string | number | undefined>,
) {
  const params = new URLSearchParams(current);
  for (const [key, next] of Object.entries(changes)) {
    if (next === undefined || next === "" || (key === "page" && next === 1))
      params.delete(key);
    else params.set(key, String(next));
  }
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}

export function TablePaginationFooter({
  pathname,
  params,
  page,
  perPage,
  total,
  label,
  onPageChange,
  onPageSizeChange,
}: {
  pathname: string;
  params: Record<string, string>;
  page: number;
  perPage: number;
  total: number;
  label: string;
  onPageChange?: (page: number) => void;
  onPageSizeChange?: (size: number) => void;
}) {
  const totalPages = total === 0 ? 0 : Math.ceil(total / perPage);
  const currentPage = totalPages === 0 ? 1 : Math.min(page, totalPages);
  const first = total === 0 ? 0 : (currentPage - 1) * perPage + 1;
  const last = Math.min(currentPage * perPage, total);

  return (
    <div className="flex flex-col items-center justify-between gap-3 border-t px-4 py-3 sm:flex-row sm:px-6 lg:px-8">
      <p className="text-sm text-muted-foreground">
        {first.toLocaleString()}–{last.toLocaleString()} of{" "}
        {total.toLocaleString()}
      </p>
      <div className="flex flex-wrap items-center justify-center gap-3 sm:justify-end">
        <PageSizeSelect pathname={pathname} params={params} value={perPage} onChange={onPageSizeChange} />
        {totalPages > 1 && (
          <nav className="flex items-center gap-1" aria-label={`${label} pages`}>
            {totalPages > 12 && (
              onPageChange ? (
                <Button type="button" variant="outline" size="icon-sm" className="hidden sm:inline-flex" onClick={() => onPageChange(1)} disabled={currentPage === 1} aria-label="First page"><ChevronsLeft /></Button>
              ) : (
                <Link
                  href={hrefWith(pathname, params, { page: 1 })}
                  aria-label="First page"
                  aria-disabled={currentPage === 1}
                  className={cn(
                    buttonVariants({ variant: "outline", size: "icon-sm" }),
                    "hidden sm:inline-flex",
                    currentPage === 1 && "pointer-events-none opacity-50",
                  )}
                ><ChevronsLeft /></Link>
              )
            )}
            {onPageChange ? (
              <Button type="button" variant="outline" size="icon-sm" onClick={() => onPageChange(Math.max(1, currentPage - 1))} disabled={currentPage === 1} aria-label="Previous page"><ChevronLeft /></Button>
            ) : (
              <Link
                href={hrefWith(pathname, params, { page: Math.max(1, currentPage - 1) })}
                aria-label="Previous page"
                aria-disabled={currentPage === 1}
                className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), currentPage === 1 && "pointer-events-none opacity-50")}
              ><ChevronLeft /></Link>
            )}
            {pageItems(currentPage, totalPages).map((item, index) =>
              item === "ellipsis" ? (
                <span
                  key={`ellipsis-${index}`}
                  className="hidden size-7 items-center justify-center text-sm text-muted-foreground sm:flex"
                  aria-hidden="true"
                >
                  …
                </span>
              ) : (
                onPageChange ? (
                  <Button
                    key={item}
                    type="button"
                    variant={item === currentPage ? "default" : "outline"}
                    size="icon-sm"
                    onClick={() => onPageChange(item)}
                    aria-label={`Page ${item}`}
                    aria-current={item === currentPage ? "page" : undefined}
                    className={Math.abs(item - currentPage) > 1 ? "hidden sm:inline-flex" : undefined}
                  >{item}</Button>
                ) : (
                  <Link
                    key={item}
                    href={hrefWith(pathname, params, { page: item })}
                    aria-label={`Page ${item}`}
                    aria-current={item === currentPage ? "page" : undefined}
                    className={cn(
                      buttonVariants({ variant: item === currentPage ? "default" : "outline", size: "icon-sm" }),
                      Math.abs(item - currentPage) > 1 && "hidden sm:inline-flex",
                    )}
                  >{item}</Link>
                )
              ),
            )}
            {onPageChange ? (
              <Button type="button" variant="outline" size="icon-sm" onClick={() => onPageChange(Math.min(totalPages, currentPage + 1))} disabled={currentPage === totalPages} aria-label="Next page"><ChevronRight /></Button>
            ) : (
              <Link
                href={hrefWith(pathname, params, { page: Math.min(totalPages, currentPage + 1) })}
                aria-label="Next page"
                aria-disabled={currentPage === totalPages}
                className={cn(buttonVariants({ variant: "outline", size: "icon-sm" }), currentPage === totalPages && "pointer-events-none opacity-50")}
              ><ChevronRight /></Link>
            )}
            {totalPages > 12 && (
              onPageChange ? (
                <Button type="button" variant="outline" size="icon-sm" className="hidden sm:inline-flex" onClick={() => onPageChange(totalPages)} disabled={currentPage === totalPages} aria-label="Last page"><ChevronsRight /></Button>
              ) : (
                <Link
                  href={hrefWith(pathname, params, { page: totalPages })}
                  aria-label="Last page"
                  aria-disabled={currentPage === totalPages}
                  className={cn(
                    buttonVariants({ variant: "outline", size: "icon-sm" }),
                    "hidden sm:inline-flex",
                    currentPage === totalPages && "pointer-events-none opacity-50",
                  )}
                ><ChevronsRight /></Link>
              )
            )}
          </nav>
        )}
      </div>
    </div>
  );
}
