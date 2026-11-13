	.set noreorder

#include "pspimport.s"

/* systemctrl of ARK-4 / ARK-5. NID = first 4 bytes of SHA-1(name), little endian. */
	IMPORT_START "SystemCtrlForKernel",0x00090000
	IMPORT_FUNC  "SystemCtrlForKernel",0x1C90BECB,sctrlHENSetStartModuleHandler
