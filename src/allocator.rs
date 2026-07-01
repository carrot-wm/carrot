// buffer allocation, vulkan-native: VK_EXT_image_drm_format_modifier for
// the images, PRIME export for scanout. no gbm - nothing in carrot links a
// C library. dumb buffers can't back accelerated scanout, don't try.
