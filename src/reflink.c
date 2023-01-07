#include <stdlib.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/ioctl.h>
#include <linux/fiemap.h>
#include <linux/fs.h>

uint64_t C_extent_addr(int fd) {
	int rc;
	struct fiemap *fm = malloc(sizeof(struct fiemap) + sizeof(struct fiemap_extent));

	fm->fm_start = 0;
	fm->fm_length = FIEMAP_MAX_OFFSET;
	fm->fm_flags = 0;
	fm->fm_extent_count = 1;

	rc = ioctl(fd, FS_IOC_FIEMAP, fm);

	if(rc || fm->fm_mapped_extents == 0) {
		free(fm);
		return 0;
	}

	uint64_t addr = fm->fm_extents[0].fe_physical;

	free(fm);
	return addr;
}

int C_is_reflink(int src_fd, int dest_fd) {
	int rc;
	struct stat src_st, dest_st;

	rc = fstat(src_fd, &src_st);
	if(rc) return rc;

	rc = fstat(dest_fd, &dest_st);
	if(rc) return rc;

	if(src_st.st_dev != dest_st.st_dev) return 1;
	if(src_st.st_ino == dest_st.st_ino) return 2;

	uint64_t src_addr = C_extent_addr(src_fd);
	if(! src_addr) return 3;

	uint64_t dest_addr = C_extent_addr(dest_fd);
	if(! dest_addr) return 4;

	return src_addr == dest_addr ? 0 : 5;
}

int C_make_reflink(int src_fd, int dest_fd) {
	return ioctl(dest_fd, FICLONE, src_fd);
}
