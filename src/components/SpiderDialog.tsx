import { useState } from "react";
import { Network } from "lucide-react";
import { api } from "../lib/api";
import { Button } from "./ui/button";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "./ui/dialog";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "./ui/select";

/// "Grab every file this page links to." The results are handed to the add
/// dialog rather than queued here: a crawl can return hundreds of links, and
/// the user should see them before the disk does.
export function SpiderDialog({
  onClose,
  onFound,
}: {
  onClose: () => void;
  /// Called with the links the crawl returned, in discovery order.
  onFound: (links: string[]) => void;
}) {
  const [url, setUrl] = useState("");
  const [depth, setDepth] = useState("0");
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (busy || !url.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const links = await api.grabLinks(url.trim(), Number(depth), filter);
      if (links.length === 0) {
        setError("No matching links on that page. Try a wider filter, or more depth.");
        setBusy(false);
        return;
      }
      onFound(links);
      onClose();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-h-[88vh] overflow-y-auto w-[calc(100vw-2rem)] sm:max-w-lg p-4 sm:p-6">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>Grab links from a page</DialogTitle>
          </DialogHeader>

          <div className="space-y-1.5">
            <Label>Page URL</Label>
            <Input
              value={url}
              onChange={(e) => setUrl(e.currentTarget.value)}
              placeholder="https://example.com/downloads"
              spellCheck={false}
              autoFocus
            />
          </div>

          <div className="space-y-1.5">
            <Label>File types (blank = every file)</Label>
            <Input
              value={filter}
              onChange={(e) => setFilter(e.currentTarget.value)}
              placeholder="zip, pdf, mp3"
              spellCheck={false}
            />
          </div>

          <div className="space-y-1.5">
            <Label>How deep</Label>
            <Select value={depth} onValueChange={setDepth}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="0">This page only</SelectItem>
                <SelectItem value="1">Follow its links one level</SelectItem>
                <SelectItem value="2">Follow two levels</SelectItem>
              </SelectContent>
            </Select>
            <p className="text-sm text-muted-foreground">
              Only pages on the same site are followed, at most 50 of them. Links
              built by JavaScript are invisible here — use the browser extension on
              those pages.
            </p>
          </div>

          {error && <p className="text-sm text-destructive">{error}</p>}

          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>Cancel</Button>
            <Button type="submit" disabled={busy || !url.trim()}>
              <Network /> {busy ? "Searching…" : "Find links"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
