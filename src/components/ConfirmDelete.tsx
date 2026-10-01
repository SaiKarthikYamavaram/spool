import { Trash2 } from "lucide-react";
import { toast } from "sonner";
import { api, type DownloadView } from "../lib/api";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "./ui/alert-dialog";

/// Removing an entry has two distinct meanings, so ask which. "Remove from
/// list" keeps the downloaded file; "Delete file too" erases it from disk.
/// For an unfinished download the file on disk is only a partial, so the
/// wording adapts.
///
/// Takes a list so a multi-row selection asks the same question once, rather
/// than needing a second near-identical dialog.
export function ConfirmDelete({
  rows,
  onClose,
}: {
  rows: DownloadView[];
  onClose: () => void;
}) {
  const many = rows.length > 1;
  // With a mixed selection the cautious wording wins: say "partial" unless
  // every entry is finished.
  const finished = rows.every((r) => r.status === "completed");
  const ids = rows.map((r) => r.id);

  function removeOnly() {
    api.bulk(ids, "remove").catch((e) => toast.error(String(e)));
    onClose();
  }
  function deleteFile() {
    api.bulk(ids, "remove_with_file").catch((e) => toast.error(String(e)));
    onClose();
  }

  return (
    <AlertDialog open onOpenChange={(open) => !open && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>
            {many ? `Remove ${rows.length} downloads?` : `Remove "${rows[0].filename}"?`}
          </AlertDialogTitle>
          <AlertDialogDescription>
            {finished
              ? many
                ? "Keep the downloaded files, or delete them from disk as well."
                : "Keep the downloaded file, or delete it from disk as well."
              : many
                ? "This cancels any that are unfinished. Their partial files can be kept or deleted."
                : "This cancels the download. The partial file can be kept or deleted."}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction variant="secondary" onClick={removeOnly}>
            Remove from list
          </AlertDialogAction>
          <AlertDialogAction variant="destructive" onClick={deleteFile}>
            <Trash2 className="size-4" /> {many ? "Delete files" : "Delete file"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
